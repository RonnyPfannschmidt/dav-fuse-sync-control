//! A real FUSE mount, over a real WebDAV server, read with ordinary syscalls.
//!
//! Everything below goes through the kernel: `std::fs::read_dir`, `File::read`,
//! `getxattr`. Nothing here calls into the crate's own types, so it is the only
//! tier that can catch a mistake in the FUSE shims themselves.
//!
//! Requires `/dev/fuse` and `rclone`. Skips politely without them, but
//! `DAVFS_E2E_REQUIRE=1` (set in CI) turns a skip into a failure, so a broken
//! runner image shows up red rather than green.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{guard::MountGuard, rclone::RcloneServer, tree};
use davfs_sync::fuse::{DavFS, OwnerIds};
use davfs_sync::remote::{RemoteStorage, WebDavBackend};
use davfs_sync::store::{ContentCache, MetaStore};
use davfs_sync::vfs::{Vfs, VfsConfig};
use davfs_sync::webdav::WebDavClient;

fn fuse_available() -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("not Linux".into());
    }
    // Existence is not enough: in an unprivileged container the node exists but
    // open() gives EPERM.
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .map_err(|e| format!("/dev/fuse: {e}"))?;
    if which("fusermount3").is_none() {
        return Err("fusermount3 not on PATH".into());
    }
    if !RcloneServer::available() {
        return Err("rclone not installed".into());
    }
    Ok(())
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

macro_rules! require_fuse {
    () => {
        if let Err(why) = fuse_available() {
            if std::env::var_os("DAVFS_E2E_REQUIRE").is_some() {
                panic!("FUSE required but unavailable: {why}");
            }
            eprintln!("SKIP {}: {why}", module_path!());
            return;
        }
    };
}

/// Mount the fixture tree and return a guard that unmounts on drop.
async fn mount() -> MountGuard {
    let fixture = tree::build().expect("fixture tree");
    let server = RcloneServer::start(fixture.path()).expect("rclone");
    // The fixture directory must outlive the server.
    std::mem::forget(fixture);

    let client = WebDavClient::new(
        server.base.to_string(),
        common::rclone::USER.into(),
        common::rclone::PASS.into(),
    )
    .expect("client");
    let remote: Arc<dyn RemoteStorage> = Arc::new(WebDavBackend::new(client));

    let data = tempfile::tempdir().expect("data dir");
    let cache = tempfile::tempdir().expect("cache dir");
    let url = format!("sqlite://{}", data.path().join("meta.db").display());

    let config = VfsConfig {
        ttl: Duration::from_secs(300),
        attr_ttl: Duration::from_secs(300),
        ..Default::default()
    };
    let meta = MetaStore::new(&url, config.ttl).await.expect("meta");
    meta.ensure_root(&remote.root(), "webdav")
        .await
        .expect("root");
    let content = ContentCache::new(cache.path()).expect("content cache");

    let vfs = Vfs::new(remote, meta, content, OwnerIds::current(), config)
        .await
        .expect("vfs");
    vfs.initial_sync(2).await.expect("initial sync");

    let mountpoint = tempfile::tempdir().expect("mountpoint");
    let fs = DavFS::new(tokio::runtime::Handle::current(), vfs);

    let mut options = fuser::Config::default();
    options.mount_options = vec![
        fuser::MountOption::FSName("davfs-sync-test".into()),
        fuser::MountOption::RO,
    ];
    options.n_threads = Some(2);

    let session = fuser::spawn_mount(fs, mountpoint.path(), &options).expect("mount");
    assert!(
        common::guard::wait_for_mount(mountpoint.path(), Duration::from_secs(10)),
        "the mount never appeared"
    );

    // These outlive the test through the guard's server field only; leak the
    // rest deliberately rather than complicating the guard.
    std::mem::forget(data);
    std::mem::forget(cache);

    MountGuard::new(session, mountpoint, server)
}

/// The runtime must be multi-threaded: the FUSE session runs on its own thread
/// and its handlers spawn onto this runtime.
macro_rules! fuse_test {
    ($name:ident, $body:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        #[serial_test::serial(fuse)]
        async fn $name() {
            require_fuse!();
            let mount = mount().await;
            let root = mount.path();
            let f: fn(std::path::PathBuf) = $body;
            tokio::task::spawn_blocking(move || f(root))
                .await
                .expect("test body");
        }
    };
}

fuse_test!(ls_lists_the_tree, |root| {
    let mut names: Vec<String> = std::fs::read_dir(&root)
        .expect("read_dir")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();

    let mut want: Vec<String> = tree::expected_root_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    want.sort();
    assert_eq!(names, want);
});

fuse_test!(cat_returns_file_content, |root| {
    // The headline change: the PoC returned ENETUNREACH from every read.
    assert_eq!(std::fs::read(root.join("plain.txt")).unwrap(), b"hello\n");
    assert_eq!(
        std::fs::read(root.join("nested/a/b/c/deep.txt")).unwrap(),
        b"deep\n"
    );
});

fuse_test!(hostile_names_are_readable_through_the_kernel, |root| {
    for (name, want) in [
        ("hash#and&.txt", &b"hash\n"[..]),
        ("apostrophe's.txt", &b"quote\n"[..]),
        ("100%done.txt", &b"percent\n"[..]),
        ("spaces in name.txt", &b"spaces\n"[..]),
        ("ümlaut-Ä.txt", &b"umlaut\n"[..]),
        ("emoji-🚀.md", &b"rocket\n"[..]),
    ] {
        let got = std::fs::read(root.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(got, want, "{name}");
    }
});

fuse_test!(a_large_file_reads_back_intact, |root| {
    let got = std::fs::read(root.join("large.bin")).unwrap();
    assert_eq!(got.len(), tree::LARGE_LEN);
    assert_eq!(got, tree::large_content());
});

fuse_test!(stat_reports_real_sizes_and_types, |root| {
    let md = std::fs::metadata(root.join("plain.txt")).unwrap();
    assert!(md.is_file());
    assert_eq!(md.len(), 6);

    let md = std::fs::metadata(root.join("large.bin")).unwrap();
    assert_eq!(md.len(), tree::LARGE_LEN as u64);

    assert!(std::fs::metadata(root.join("empty")).unwrap().is_dir());
    assert!(std::fs::metadata(root.join("nested/a/b/c"))
        .unwrap()
        .is_dir());
});

// The PoC reported every timestamp as the Unix epoch.
fuse_test!(stat_reports_a_real_mtime, |root| {
    use std::time::{Duration, UNIX_EPOCH};
    let md = std::fs::metadata(root.join(tree::KNOWN_MTIME_FILE)).unwrap();
    let mtime = md.modified().unwrap();

    assert_ne!(mtime, UNIX_EPOCH, "mtime is still the epoch");
    let expected = UNIX_EPOCH + Duration::from_secs(tree::KNOWN_MTIME_UNIX as u64);
    let delta = mtime
        .duration_since(expected)
        .or_else(|_| expected.duration_since(mtime))
        .unwrap();
    assert!(delta <= Duration::from_secs(1), "mtime off by {delta:?}");
});

fuse_test!(a_directory_does_not_contain_itself, |root| {
    // rclone's Depth:1 self-entry is named after the directory, so the PoC
    // showed `c` inside `c`, recursively.
    let names: Vec<String> = std::fs::read_dir(root.join("nested/a/b/c"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["deep.txt"]);
});

fuse_test!(an_empty_directory_is_empty, |root| {
    assert_eq!(std::fs::read_dir(root.join("empty")).unwrap().count(), 0);
});

fuse_test!(nested_directories_are_walkable, |root| {
    let mut found = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                walk(&entry.path(), out);
            } else {
                out.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    walk(&root.join("nested"), &mut found);
    assert_eq!(found, vec!["deep.txt"]);
});

fuse_test!(missing_paths_report_enoent, |root| {
    let err = std::fs::read(root.join("does-not-exist.txt")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
});

fuse_test!(the_mount_is_read_only, |root| {
    // EROFS, not ENOSYS: ENOSYS would make the kernel disable the operation for
    // the lifetime of the mount.
    let err = std::fs::write(root.join("new.txt"), b"nope").unwrap_err();
    assert!(
        matches!(err.raw_os_error(), Some(libc::EROFS) | Some(libc::EACCES)),
        "unexpected error: {err:?} ({:?})",
        err.raw_os_error()
    );
});

fuse_test!(xattrs_are_readable_through_the_kernel, |root| {
    let path = root.join("plain.txt");

    let state = xattr_get(&path, "user.davfs.state");
    assert!(
        state == "cloud" || state == "cached",
        "unexpected state {state:?}"
    );

    // Reading the file makes the content local.
    let _ = std::fs::read(&path).unwrap();
    assert_eq!(xattr_get(&path, "user.davfs.state"), "cached");

    // A directory reports a state too -- which the PoC could never do.
    let dir_state = xattr_get(&root.join("nested"), "user.davfs.state");
    assert!(
        dir_state == "cached" || dir_state == "cloud",
        "unexpected directory state {dir_state:?}"
    );

    assert!(xattr_list(&path).contains(&"user.davfs.state".to_string()));
});

// The size-protocol edge: a buffer that is too small must give ERANGE.
fuse_test!(getxattr_reports_erange_for_a_short_buffer, |root| {
    let path = root.join("plain.txt");
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let name = std::ffi::CString::new("user.davfs.state").unwrap();

    let mut buf = [0u8; 1];
    // Safe: both pointers are valid and the length matches the buffer.
    let rc = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ERANGE)
    );
});

fuse_test!(the_mountpoint_is_a_distinct_filesystem, |root| {
    use std::os::unix::fs::MetadataExt;
    let inside = std::fs::metadata(&root).unwrap().dev();
    let outside = std::fs::metadata(root.parent().unwrap()).unwrap().dev();
    assert_ne!(inside, outside, "the mount is not actually mounted");
});

// Concurrent reads must not deadlock or interleave wrongly. Before the
// spawn-per-request change every FUSE operation was serialized on one thread.
fuse_test!(concurrent_reads_all_succeed, |root| {
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || {
                assert_eq!(std::fs::read(root.join("plain.txt")).unwrap(), b"hello\n");
                let _ = std::fs::read_dir(root.join("nested")).unwrap().count();
            })
        })
        .collect();
    for h in handles {
        h.join().expect("a concurrent reader panicked");
    }
});

fn xattr_get(path: &std::path::Path, name: &str) -> String {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let c_name = std::ffi::CString::new(name).unwrap();
    let mut buf = vec![0u8; 256];
    // Safe: both pointers are valid and the length matches the buffer.
    let n = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            c_name.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        )
    };
    assert!(
        n >= 0,
        "getxattr({name}) failed: {}",
        std::io::Error::last_os_error()
    );
    buf.truncate(n as usize);
    String::from_utf8(buf).unwrap()
}

fn xattr_list(path: &std::path::Path) -> Vec<String> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let mut buf = vec![0u8; 1024];
    // Safe: pointer and length describe `buf`.
    let n = unsafe {
        libc::listxattr(
            c_path.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
        )
    };
    assert!(n >= 0, "listxattr failed");
    buf.truncate(n as usize);
    buf.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}
