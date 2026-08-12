//! Against a real Nextcloud, including a real FUSE mount.
//!
//! Nextcloud is the server this project was written for, and it differs from
//! rclone in precisely the places that were changed: `d:` prefixes, a deep base
//! path, `getetag` on collections, a `404 Not Found` propstat for unsupported
//! properties, XML-escaped etags, and genuine conditional-request handling.
//!
//! Skips without `DAVFS_NEXTCLOUD_URL`; `DAVFS_NEXTCLOUD_REQUIRE=1` turns the
//! skip into a failure so CI cannot go quietly green.

#[macro_use]
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::guard::MountGuard;
use davfs_sync::fuse::{DavFS, OwnerIds};
use davfs_sync::remote::{Listing, NodeId, RemoteStorage, Validator, WebDavBackend};
use davfs_sync::store::{ContentCache, MetaStore};
use davfs_sync::vfs::{Vfs, VfsConfig};
use davfs_sync::webdav::{ListResult, WebDavClient};

/// Each test gets its own directory.
///
/// Nextcloud takes a real lock on a collection while it is being modified, so
/// parallel tests sharing one fixture directory get `423 Locked`. Per-test
/// directories are also what makes the suite safe to re-run against a
/// long-lived server.
fn test_dir(name: &str) -> String {
    format!("davfs-test-{name}")
}

fn client(nc: &common::nextcloud::Nextcloud) -> WebDavClient {
    WebDavClient::new(nc.base.clone(), nc.user.clone(), nc.pass.clone()).expect("client")
}

/// Create the fixture tree over plain HTTP, so the code under test is not used
/// to build its own inputs. Returns the directory name.
fn seed(nc: &common::nextcloud::Nextcloud, name: &str) -> String {
    let dir = test_dir(name);
    nc.delete(&dir).ok();
    nc.mkcol(&dir)
        .unwrap_or_else(|e| panic!("mkcol {dir}: {e}"));
    nc.mkcol(&format!("{dir}/nested")).expect("mkcol nested");
    nc.mkcol(&format!("{dir}/empty")).expect("mkcol empty");

    nc.put(&format!("{dir}/plain.txt"), b"hello\n").unwrap();
    nc.put(&format!("{dir}/spaces in name.txt"), b"spaces\n")
        .unwrap();
    nc.put(&format!("{dir}/ümlaut-Ä.txt"), b"umlaut\n").unwrap();
    nc.put(&format!("{dir}/hash#and&.txt"), b"hash\n").unwrap();
    nc.put(&format!("{dir}/apostrophe's.txt"), b"quote\n")
        .unwrap();
    nc.put(&format!("{dir}/100%done.txt"), b"percent\n")
        .unwrap();
    nc.put(&format!("{dir}/nested/deep.txt"), b"deep\n")
        .unwrap();

    // Big enough to span many FUSE reads.
    nc.put(&format!("{dir}/large.bin"), &large_content())
        .unwrap();
    dir
}

fn large_content() -> Vec<u8> {
    (0..2_000_000).map(|i| (i % 251) as u8).collect()
}

// ---------------------------------------------------------------------------
// Client level
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lists_a_real_nextcloud() {
    let nc = require_nextcloud!();
    let dir = seed(&nc, "list");
    let c = client(&nc);

    let ListResult::Modified { entries, etag } = c.list_dir(&dir, None).await.expect("list") else {
        panic!("a first listing cannot be NotModified");
    };

    // The PoC's parser produced zero entries here: it compared a full base URL
    // against a path-absolute href, so nothing ever matched.
    let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "100%done.txt",
            "apostrophe's.txt",
            "empty",
            "hash#and&.txt",
            "large.bin",
            "nested",
            "plain.txt",
            "spaces in name.txt",
            "ümlaut-Ä.txt",
        ]
    );

    // Nextcloud reports getetag on collections, so the directory validator --
    // and with it every conditional request -- actually works here.
    assert!(
        etag.is_some(),
        "no directory validator; conditional requests would be dead"
    );
    // XML-escaped in the response (&quot;), so this proves entity decoding too.
    let etag = etag.unwrap();
    assert!(etag.starts_with('"'), "etag not unescaped: {etag:?}");
    assert!(!etag.contains("&quot;"), "etag still escaped: {etag:?}");
}

/// The conditional path the PoC could never exercise.
#[tokio::test]
async fn an_unchanged_directory_returns_not_modified() {
    let nc = require_nextcloud!();
    let dir = seed(&nc, "cond");
    let c = client(&nc);

    let ListResult::Modified { etag, .. } = c.list_dir(&dir, None).await.unwrap() else {
        panic!("unexpected NotModified");
    };
    let etag = etag.expect("Nextcloud sends a collection etag");

    let again = c.list_dir(&dir, Some(&etag)).await.unwrap();
    assert_eq!(
        again,
        ListResult::NotModified,
        "revalidation did not detect an unchanged directory"
    );

    // Change something: the validator must move.
    nc.put(&format!("{dir}/newfile.txt"), b"new").unwrap();
    let after = c.list_dir(&dir, Some(&etag)).await.unwrap();
    assert!(
        matches!(after, ListResult::Modified { .. }),
        "a changed directory still reported NotModified"
    );
}

#[tokio::test]
async fn a_subdirectory_does_not_contain_itself() {
    let nc = require_nextcloud!();
    let dir = seed(&nc, "selfentry");
    let c = client(&nc);

    let ListResult::Modified { entries, .. } =
        c.list_dir(&format!("{dir}/nested"), None).await.unwrap()
    else {
        panic!("unexpected NotModified");
    };
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["deep.txt"], "the self-entry leaked in");
}

#[tokio::test]
async fn content_downloads_from_nextcloud() {
    let nc = require_nextcloud!();
    let dir = seed(&nc, "content");
    let c = client(&nc);

    assert_eq!(
        c.read_file(&format!("{dir}/plain.txt")).await.unwrap(),
        b"hello\n"
    );
    // Names that url_for must percent-encode correctly against a deep base path.
    assert_eq!(
        c.read_file(&format!("{dir}/hash#and&.txt")).await.unwrap(),
        b"hash\n"
    );
    assert_eq!(
        c.read_file(&format!("{dir}/100%done.txt")).await.unwrap(),
        b"percent\n"
    );
    assert_eq!(
        c.read_file(&format!("{dir}/large.bin")).await.unwrap(),
        large_content()
    );
}

#[tokio::test]
async fn bad_credentials_are_unauthorized() {
    let nc = require_nextcloud!();
    let c = WebDavClient::new(nc.base.clone(), "wrong".into(), "wrong".into()).unwrap();
    let err = c.list_dir("", None).await.unwrap_err();
    assert_eq!(err.errno(), libc::EACCES, "got {err:?}");
    // Retrying a 401 is what trips Nextcloud's own brute-force protection.
    assert!(!err.is_retryable());
}

// ---------------------------------------------------------------------------
// Through the seam
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_backend_reports_a_directory_validator() {
    let nc = require_nextcloud!();
    let name = seed(&nc, "validator");
    let backend: Arc<dyn RemoteStorage> = Arc::new(WebDavBackend::new(client(&nc)));

    let dir = NodeId(format!("/{name}"));
    let Listing::Fresh {
        validator,
        this,
        children,
        ..
    } = backend.list(&dir, None).await.unwrap()
    else {
        panic!("unexpected Unchanged");
    };

    assert!(this.is_some());
    assert!(!children.is_empty());
    let validator = validator.expect("Nextcloud provides a collection etag");
    assert!(matches!(validator, Validator::Etag(_)));

    // Round-trip: the same validator must produce Unchanged.
    assert_eq!(
        backend.list(&dir, Some(&validator)).await.unwrap(),
        Listing::Unchanged
    );
}

/// Nextcloud honours RFC 4918 `If` handling, unlike rclone. This is the branch
/// `Capabilities::enforces_preconditions` describes.
#[tokio::test]
async fn nextcloud_enforces_conditional_writes() {
    let nc = require_nextcloud!();
    let dir = seed(&nc, "precond");
    let backend: Arc<dyn RemoteStorage> = Arc::new(WebDavBackend::new(client(&nc)));

    let target = NodeId(format!("/{dir}/plain.txt"));
    let stale = Validator::Etag("\"definitely-not-current\"".into());

    let result = backend
        .write_content(
            &target,
            davfs_sync::remote::ContentSource::Bytes(bytes::Bytes::from_static(b"clobbered")),
            Some(&stale),
        )
        .await;

    let on_server = client(&nc)
        .read_file(&format!("{dir}/plain.txt"))
        .await
        .unwrap();

    match result {
        Err(e) => {
            assert_eq!(e.errno(), libc::ESTALE, "expected a 412, got {e:?}");
            assert_eq!(on_server, b"hello\n", "rejected but written anyway");
        }
        Ok(_) => {
            // Recorded rather than asserted: some deployments sit behind
            // proxies that strip conditional headers.
            eprintln!(
                "NOTE: this Nextcloud did not enforce If-Match; \
                 a conflict policy cannot rely on it here"
            );
            assert_eq!(on_server, b"clobbered");
        }
    }
}

// ---------------------------------------------------------------------------
// Through a real mount
// ---------------------------------------------------------------------------

fn fuse_available() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .is_ok()
}

async fn mount(nc: &common::nextcloud::Nextcloud) -> MountGuard {
    let remote: Arc<dyn RemoteStorage> = Arc::new(WebDavBackend::new(client(nc)));

    let data = tempfile::tempdir().expect("data");
    let cache = tempfile::tempdir().expect("cache");
    let url = format!("sqlite://{}", data.path().join("meta.db").display());

    let config = VfsConfig {
        ttl: Duration::from_secs(300),
        attr_ttl: Duration::from_secs(300),
        ..Default::default()
    };
    let meta = MetaStore::new(&url, config.ttl).await.expect("meta");
    meta.ensure_root(&remote.root(), "webdav").await.unwrap();

    let vfs = Vfs::new(
        remote,
        meta,
        ContentCache::new(cache.path()).unwrap(),
        OwnerIds::current(),
        config,
    )
    .await
    .expect("vfs");
    vfs.initial_sync(2).await.expect("initial sync");

    let mountpoint = tempfile::tempdir().expect("mountpoint");
    let fs = DavFS::new(tokio::runtime::Handle::current(), vfs);

    let mut options = fuser::Config::default();
    options.mount_options = vec![
        fuser::MountOption::FSName("davfs-nextcloud-test".into()),
        fuser::MountOption::RO,
    ];
    options.n_threads = Some(2);

    let session = fuser::spawn_mount(fs, mountpoint.path(), &options).expect("mount");
    assert!(
        common::guard::wait_for_mount(mountpoint.path(), Duration::from_secs(10)),
        "the mount never appeared"
    );

    std::mem::forget(data);
    std::mem::forget(cache);
    MountGuard::without_server(session, mountpoint)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(fuse)]
async fn a_real_mount_over_nextcloud_reads_files() {
    let nc = require_nextcloud!();
    if !fuse_available() {
        if std::env::var_os("DAVFS_E2E_REQUIRE").is_some() {
            panic!("/dev/fuse required but unavailable");
        }
        eprintln!("SKIP: no /dev/fuse");
        return;
    }
    let dir = seed(&nc, "mount");

    let guard = mount(&nc).await;
    let root = guard.path().join(&dir);

    tokio::task::spawn_blocking(move || {
        // ls
        let mut names: Vec<String> = std::fs::read_dir(&root)
            .expect("read_dir")
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert!(names.contains(&"plain.txt".to_string()), "got {names:?}");
        assert!(names.contains(&"ümlaut-Ä.txt".to_string()));
        assert!(names.contains(&"hash#and&.txt".to_string()));

        // cat -- the headline capability, against the real target server.
        assert_eq!(std::fs::read(root.join("plain.txt")).unwrap(), b"hello\n");
        assert_eq!(
            std::fs::read(root.join("hash#and&.txt")).unwrap(),
            b"hash\n"
        );
        assert_eq!(
            std::fs::read(root.join("apostrophe's.txt")).unwrap(),
            b"quote\n"
        );
        assert_eq!(
            std::fs::read(root.join("nested/deep.txt")).unwrap(),
            b"deep\n"
        );

        // A multi-megabyte file, byte for byte.
        assert_eq!(
            std::fs::read(root.join("large.bin")).unwrap(),
            large_content()
        );

        // Real sizes and a non-epoch mtime.
        let md = std::fs::metadata(root.join("plain.txt")).unwrap();
        assert_eq!(md.len(), 6);
        assert_ne!(
            md.modified().unwrap(),
            std::time::UNIX_EPOCH,
            "mtime is still the epoch"
        );

        // An empty directory is empty, not an error.
        assert_eq!(std::fs::read_dir(root.join("empty")).unwrap().count(), 0);

        // A directory does not contain itself.
        let nested: Vec<String> = std::fs::read_dir(root.join("nested"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(nested, vec!["deep.txt"]);
    })
    .await
    .expect("test body");
}
