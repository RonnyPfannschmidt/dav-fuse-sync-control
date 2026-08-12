//! The filesystem core, without a mount.
//!
//! Everything interesting -- caching, staleness, offline behaviour, the content
//! cache, xattr actions -- is exercised here against `MockRemote` and a
//! tempfile database. No `/dev/fuse`, no root, no server, and fast enough to
//! run on every save.

use std::sync::Arc;
use std::time::Duration;

use davfs_sync::clock::Clock;
use davfs_sync::fuse::OwnerIds;
use davfs_sync::remote::{mock::MockRemote, NodeId, RemoteStorage};
use davfs_sync::store::{ContentCache, MetaStore};
use davfs_sync::vfs::{OnOpenUncached, Vfs, VfsConfig};

const OWNER: OwnerIds = OwnerIds {
    uid: 1000,
    gid: 1000,
};

struct Harness {
    vfs: Arc<Vfs>,
    mock: Arc<MockRemote>,
    clock: Clock,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn harness_with(mock: MockRemote, config: VfsConfig) -> Harness {
    let mock = Arc::new(mock);
    let data = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let clock = Clock::fixed(1_000_000);

    let url = format!("sqlite://{}", data.path().join("meta.db").display());
    let meta = MetaStore::with_clock(&url, config.ttl, clock.clone())
        .await
        .unwrap();
    meta.ensure_root(&mock.root(), "mock").await.unwrap();
    let content = ContentCache::new(cache.path()).unwrap();

    let vfs = Vfs::new(
        mock.clone() as Arc<dyn RemoteStorage>,
        meta,
        content,
        OWNER,
        config,
    )
    .await
    .unwrap();

    Harness {
        vfs,
        mock,
        clock,
        _dirs: (data, cache),
    }
}

/// A small tree: /hello.txt, /sub/, /sub/inner.txt
async fn harness() -> Harness {
    let mock = MockRemote::new();
    mock.add_file("/", "hello.txt", b"hello world\n");
    mock.add_dir("/", "sub");
    mock.add_file("/sub", "inner.txt", b"inner\n");
    harness_with(mock, VfsConfig::default()).await
}

const ROOT: u64 = 1;

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

#[tokio::test]
async fn readdir_lists_the_root() {
    let h = harness().await;
    let entries = h.vfs.readdir(ROOT).await.unwrap();

    let mut names: Vec<&str> = entries.iter().map(|(_, _, n)| n.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec![".", "..", "hello.txt", "sub"]);
}

#[tokio::test]
async fn a_directory_does_not_contain_itself() {
    let h = harness().await;
    let sub = h.vfs.lookup(ROOT, "sub").await.unwrap();
    let entries = h.vfs.readdir(sub.ino.0).await.unwrap();

    let names: Vec<&str> = entries
        .iter()
        .map(|(_, _, n)| n.as_str())
        .filter(|n| *n != "." && *n != "..")
        .collect();
    assert_eq!(names, vec!["inner.txt"]);
}

#[tokio::test]
async fn lookup_returns_attributes_and_a_stable_inode() {
    let h = harness().await;
    let a = h.vfs.lookup(ROOT, "hello.txt").await.unwrap();
    assert_eq!(a.size, 12);
    assert_eq!(a.kind, fuser::FileType::RegularFile);
    assert_eq!((a.uid, a.gid), (1000, 1000));

    // The same node keeps the same inode.
    let b = h.vfs.lookup(ROOT, "hello.txt").await.unwrap();
    assert_eq!(a.ino, b.ino);
    assert_eq!(h.vfs.getattr(a.ino.0).await.unwrap().ino, a.ino);
}

/// Inodes must survive a remount, which the PoC's in-memory counter did not.
#[tokio::test]
async fn inodes_are_stable_across_a_remount() {
    let mock = Arc::new(MockRemote::new());
    mock.add_file("/", "a.txt", b"a");
    mock.add_file("/", "b.txt", b"b");

    let data = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", data.path().join("meta.db").display());

    let mut first = Vec::new();
    for round in 0..2 {
        let cache = tempfile::tempdir().unwrap();
        let meta = MetaStore::new(&url, Duration::from_secs(30)).await.unwrap();
        meta.ensure_root(&mock.root(), "mock").await.unwrap();
        let vfs = Vfs::new(
            mock.clone() as Arc<dyn RemoteStorage>,
            meta.clone(),
            ContentCache::new(cache.path()).unwrap(),
            OWNER,
            VfsConfig::default(),
        )
        .await
        .unwrap();

        let a = vfs.lookup(ROOT, "a.txt").await.unwrap().ino;
        let b = vfs.lookup(ROOT, "b.txt").await.unwrap().ino;
        if round == 0 {
            first = vec![a, b];
        } else {
            assert_eq!(vec![a, b], first, "inodes changed across remount");
        }
        meta.close().await;
    }
}

#[tokio::test]
async fn missing_names_are_enoent() {
    let h = harness().await;
    let err = h.vfs.lookup(ROOT, "nope.txt").await.unwrap_err();
    assert_eq!(err.errno(), libc::ENOENT);
}

// ---------------------------------------------------------------------------
// Caching and staleness
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_fresh_listing_is_served_without_touching_the_backend() {
    let h = harness().await;
    h.vfs.readdir(ROOT).await.unwrap();
    let after_first = h.mock.list_calls.load(std::sync::atomic::Ordering::SeqCst);

    for _ in 0..5 {
        h.vfs.readdir(ROOT).await.unwrap();
    }
    assert_eq!(
        h.mock.list_calls.load(std::sync::atomic::Ordering::SeqCst),
        after_first,
        "a fresh cache should not produce requests"
    );
}

#[tokio::test]
async fn a_stale_listing_is_served_immediately_and_refreshed_behind_it() {
    let h = harness().await;
    h.vfs.readdir(ROOT).await.unwrap();
    let before = h.mock.list_calls.load(std::sync::atomic::Ordering::SeqCst);

    h.clock.advance(3600);
    // Still answered from cache -- the point of stale-while-revalidate.
    let entries = h.vfs.readdir(ROOT).await.unwrap();
    assert_eq!(entries.len(), 4);

    // The refresh happens on a spawned task.
    for _ in 0..50 {
        if h.mock.list_calls.load(std::sync::atomic::Ordering::SeqCst) > before {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no background refresh was scheduled");
}

/// A deleted file must disappear, which the PoC could not express: it signalled
/// 304 with an empty Vec and then guarded the write-back with `!is_empty()`.
#[tokio::test]
async fn deletions_propagate_on_refresh() {
    let h = harness().await;
    h.vfs.readdir(ROOT).await.unwrap();

    h.mock.remove_node(&NodeId("/hello.txt".into()));
    h.clock.advance(3600);

    // Force a synchronous refresh via the action attribute.
    h.vfs
        .setxattr(ROOT, "user.davfs.do_refresh", b"1")
        .await
        .unwrap();

    let names: Vec<String> = h
        .vfs
        .readdir(ROOT)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, _, n)| n)
        .collect();
    assert!(!names.contains(&"hello.txt".to_string()), "got {names:?}");
    assert!(names.contains(&"sub".to_string()));
}

/// Emptying a directory entirely must also propagate.
#[tokio::test]
async fn a_directory_emptied_remotely_becomes_empty_locally() {
    let mock = MockRemote::new();
    mock.add_dir("/", "d");
    mock.add_file("/d", "only.txt", b"x");
    let h = harness_with(mock, VfsConfig::default()).await;

    let d = h.vfs.lookup(ROOT, "d").await.unwrap();
    assert_eq!(h.vfs.readdir(d.ino.0).await.unwrap().len(), 3); // . .. only.txt

    h.mock.remove_node(&NodeId("/d/only.txt".into()));
    h.vfs
        .setxattr(d.ino.0, "user.davfs.do_refresh", b"1")
        .await
        .unwrap();

    assert_eq!(
        h.vfs.readdir(d.ino.0).await.unwrap().len(),
        2,
        "an emptied directory still reports children"
    );
}

/// Cached listings must survive the server going away.
#[tokio::test]
async fn cached_listings_are_served_while_offline() {
    let h = harness().await;
    h.vfs.readdir(ROOT).await.unwrap();
    let sub = h.vfs.lookup(ROOT, "sub").await.unwrap();
    h.vfs.readdir(sub.ino.0).await.unwrap();

    h.mock.go_offline();

    assert_eq!(h.vfs.readdir(ROOT).await.unwrap().len(), 4);
    assert_eq!(h.vfs.readdir(sub.ino.0).await.unwrap().len(), 3);
    assert!(h.vfs.lookup(ROOT, "hello.txt").await.is_ok());
}

/// An unvisited directory while offline must be ENETUNREACH, never ENOENT.
#[tokio::test]
async fn an_uncached_listing_while_offline_is_not_reported_as_missing() {
    let mock = MockRemote::new();
    mock.add_dir("/", "never_visited");
    mock.add_file("/never_visited", "x.txt", b"x");
    let h = harness_with(mock, VfsConfig::default()).await;

    // Cache the root only.
    h.vfs.readdir(ROOT).await.unwrap();
    let dir = h.vfs.lookup(ROOT, "never_visited").await.unwrap();

    h.mock.go_offline();

    let err = h.vfs.readdir(dir.ino.0).await.unwrap_err();
    assert_eq!(
        err.errno(),
        libc::ENETUNREACH,
        "an unreachable server must not look like a missing directory"
    );
    assert_ne!(err.errno(), libc::ENOENT);
}

// ---------------------------------------------------------------------------
// Content
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_returns_file_content() {
    // The headline change: the PoC returned ENETUNREACH here, always.
    let h = harness().await;
    let f = h.vfs.lookup(ROOT, "hello.txt").await.unwrap();

    let fh = h.vfs.open(f.ino.0, false).await.unwrap();
    let data = h.vfs.read(fh, 0, 4096).await.unwrap();
    assert_eq!(data, b"hello world\n");
    h.vfs.release(fh);
}

#[tokio::test]
async fn reads_are_positional_and_clamp_at_eof() {
    let h = harness().await;
    let f = h.vfs.lookup(ROOT, "hello.txt").await.unwrap();
    let fh = h.vfs.open(f.ino.0, false).await.unwrap();

    assert_eq!(h.vfs.read(fh, 6, 5).await.unwrap(), b"world");
    assert_eq!(h.vfs.read(fh, 0, 5).await.unwrap(), b"hello");
    // Past the end is an empty read, not an error.
    assert!(h.vfs.read(fh, 9999, 10).await.unwrap().is_empty());
    // A short tail returns only what exists.
    assert_eq!(h.vfs.read(fh, 11, 100).await.unwrap(), b"\n");
}

#[tokio::test]
async fn a_large_file_reads_back_byte_for_byte() {
    let mock = MockRemote::new();
    let content: Vec<u8> = (0..1_000_000).map(|i| (i % 251) as u8).collect();
    mock.add_file("/", "big.bin", &content);
    let h = harness_with(mock, VfsConfig::default()).await;

    let f = h.vfs.lookup(ROOT, "big.bin").await.unwrap();
    let fh = h.vfs.open(f.ino.0, false).await.unwrap();

    // In 128 KiB chunks, as the kernel would.
    let mut got = Vec::new();
    let mut offset = 0u64;
    loop {
        let chunk = h.vfs.read(fh, offset, 131_072).await.unwrap();
        if chunk.is_empty() {
            break;
        }
        offset += chunk.len() as u64;
        got.extend_from_slice(&chunk);
    }
    assert_eq!(got, content);
}

/// Concurrent opens of one uncached file must produce exactly one download.
#[tokio::test]
async fn downloads_are_single_flighted() {
    let h = harness().await;
    let f = h.vfs.lookup(ROOT, "hello.txt").await.unwrap();

    let handles: Vec<_> = (0..20)
        .map(|_| {
            let vfs = h.vfs.clone();
            let ino = f.ino.0;
            tokio::spawn(async move { vfs.open(ino, false).await })
        })
        .collect();

    for handle in handles {
        let fh = handle.await.unwrap().unwrap();
        assert_eq!(h.vfs.read(fh, 0, 64).await.unwrap(), b"hello world\n");
    }

    assert_eq!(
        h.mock.read_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "twenty concurrent opens should produce one GET"
    );
}

#[tokio::test]
async fn a_second_open_is_served_from_the_blob_cache() {
    let h = harness().await;
    let f = h.vfs.lookup(ROOT, "hello.txt").await.unwrap();

    let fh = h.vfs.open(f.ino.0, false).await.unwrap();
    h.vfs.release(fh);
    let after_first = h.mock.read_calls.load(std::sync::atomic::Ordering::SeqCst);

    let fh = h.vfs.open(f.ino.0, false).await.unwrap();
    assert_eq!(h.vfs.read(fh, 0, 64).await.unwrap(), b"hello world\n");
    assert_eq!(
        h.mock.read_calls.load(std::sync::atomic::Ordering::SeqCst),
        after_first,
        "the blob cache was not used"
    );
}

/// Cached content must remain readable with the server gone. This is the whole
/// point of the project.
#[tokio::test]
async fn cached_content_reads_while_offline() {
    let h = harness().await;
    let f = h.vfs.lookup(ROOT, "hello.txt").await.unwrap();
    let fh = h.vfs.open(f.ino.0, false).await.unwrap();
    h.vfs.release(fh);

    h.mock.go_offline();

    let fh = h.vfs.open(f.ino.0, false).await.unwrap();
    assert_eq!(h.vfs.read(fh, 0, 64).await.unwrap(), b"hello world\n");
}

#[tokio::test]
async fn opening_an_uncached_file_while_offline_is_enetunreach() {
    let h = harness().await;
    h.vfs.readdir(ROOT).await.unwrap();
    let f = h.vfs.lookup(ROOT, "hello.txt").await.unwrap();

    h.mock.go_offline();

    let err = h.vfs.open(f.ino.0, false).await.unwrap_err();
    assert_eq!(err.errno(), libc::ENETUNREACH, "got {err:?}");
}

#[tokio::test]
async fn on_open_uncached_error_refuses_to_fetch() {
    let mock = MockRemote::new();
    mock.add_file("/", "a.txt", b"content");
    let h = harness_with(
        mock,
        VfsConfig {
            on_open_uncached: OnOpenUncached::Error,
            ..Default::default()
        },
    )
    .await;

    let f = h.vfs.lookup(ROOT, "a.txt").await.unwrap();
    assert!(h.vfs.open(f.ino.0, false).await.is_err());
    assert_eq!(
        h.mock.read_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "Error mode must not fetch anything"
    );
}

#[tokio::test]
async fn on_open_uncached_background_defers_the_wait_to_read() {
    let mock = MockRemote::new();
    mock.add_file("/", "a.txt", b"deferred content");
    let h = harness_with(
        mock,
        VfsConfig {
            on_open_uncached: OnOpenUncached::Background,
            ..Default::default()
        },
    )
    .await;

    let f = h.vfs.lookup(ROOT, "a.txt").await.unwrap();
    let fh = h.vfs.open(f.ino.0, false).await.unwrap();
    // The download completes by the time the first read returns.
    assert_eq!(h.vfs.read(fh, 0, 64).await.unwrap(), b"deferred content");
}

/// A changed file must not serve stale bytes: the cache key includes the
/// validator, so a new version cannot hit an old blob.
#[tokio::test]
async fn a_changed_file_is_refetched_rather_than_served_stale() {
    let h = harness().await;
    let f = h.vfs.lookup(ROOT, "hello.txt").await.unwrap();
    let fh = h.vfs.open(f.ino.0, false).await.unwrap();
    assert_eq!(h.vfs.read(fh, 0, 64).await.unwrap(), b"hello world\n");
    h.vfs.release(fh);

    h.mock
        .set_content(&NodeId("/hello.txt".into()), b"REPLACED");
    h.clock.advance(3600);
    h.vfs
        .setxattr(ROOT, "user.davfs.do_refresh", b"1")
        .await
        .unwrap();

    let f = h.vfs.lookup(ROOT, "hello.txt").await.unwrap();
    let fh = h.vfs.open(f.ino.0, false).await.unwrap();
    assert_eq!(
        h.vfs.read(fh, 0, 64).await.unwrap(),
        b"REPLACED",
        "a stale blob was served for a changed file"
    );
}

// ---------------------------------------------------------------------------
// Extended attributes
// ---------------------------------------------------------------------------

/// The Nautilus extension depends on this exact vocabulary.
#[tokio::test]
async fn state_xattr_keeps_its_contract() {
    let h = harness().await;
    h.vfs.readdir(ROOT).await.unwrap();

    let state = |ino| {
        let vfs = h.vfs.clone();
        async move { String::from_utf8(vfs.getxattr(ino, "user.davfs.state").await.unwrap()).unwrap() }
    };

    // A freshly listed directory is `cached` -- which the PoC could never
    // report, because it decided dir-vs-file with `path.ends_with('/')`.
    assert_eq!(state(ROOT).await, "cached");

    let f = h.vfs.lookup(ROOT, "hello.txt").await.unwrap();
    assert_eq!(state(f.ino.0).await, "cloud", "content is not local yet");

    let fh = h.vfs.open(f.ino.0, false).await.unwrap();
    h.vfs.release(fh);
    assert_eq!(state(f.ino.0).await, "cached", "content is local now");
}

#[tokio::test]
async fn a_stale_directory_reports_cloud() {
    let h = harness().await;
    h.vfs.readdir(ROOT).await.unwrap();
    h.clock.advance(3600);
    let v = h.vfs.getxattr(ROOT, "user.davfs.state").await.unwrap();
    assert_eq!(String::from_utf8(v).unwrap(), "cloud");
}

#[tokio::test]
async fn unknown_attributes_are_rejected_distinctly() {
    let h = harness().await;
    let err = h.vfs.getxattr(ROOT, "user.davfs.nope").await.unwrap_err();
    // The shim turns NotFound into ENODATA: the file exists, the attribute does not.
    assert_eq!(err.errno(), libc::ENOENT);
}

#[tokio::test]
async fn listxattr_advertises_every_readable_attribute() {
    let h = harness().await;
    let buf = h.vfs.listxattr();
    let names: Vec<&str> = buf
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| std::str::from_utf8(s).unwrap())
        .collect();

    assert!(names.contains(&"user.davfs.state"));
    for name in &names {
        assert!(
            h.vfs.getxattr(ROOT, name).await.is_ok(),
            "{name} is advertised but not readable"
        );
    }
}

#[tokio::test]
async fn do_download_and_do_free_round_trip() {
    let h = harness().await;
    let f = h.vfs.lookup(ROOT, "hello.txt").await.unwrap();
    let state = |ino| {
        let vfs = h.vfs.clone();
        async move { String::from_utf8(vfs.getxattr(ino, "user.davfs.state").await.unwrap()).unwrap() }
    };

    h.vfs
        .setxattr(f.ino.0, "user.davfs.do_download", b"1")
        .await
        .unwrap();
    assert_eq!(state(f.ino.0).await, "cached");

    h.vfs
        .setxattr(f.ino.0, "user.davfs.do_free", b"1")
        .await
        .unwrap();
    assert_eq!(state(f.ino.0).await, "cloud");
}

#[tokio::test]
async fn pinned_content_cannot_be_freed() {
    let h = harness().await;
    let f = h.vfs.lookup(ROOT, "hello.txt").await.unwrap();

    h.vfs
        .setxattr(f.ino.0, "user.davfs.do_download", b"1")
        .await
        .unwrap();
    h.vfs
        .setxattr(f.ino.0, "user.davfs.do_pin", b"1")
        .await
        .unwrap();

    let pinned = h.vfs.getxattr(f.ino.0, "user.davfs.pinned").await.unwrap();
    assert_eq!(pinned, b"1");
    assert!(
        h.vfs
            .setxattr(f.ino.0, "user.davfs.do_free", b"1")
            .await
            .is_err(),
        "a pinned file was freed"
    );

    h.vfs
        .setxattr(f.ino.0, "user.davfs.do_unpin", b"1")
        .await
        .unwrap();
    h.vfs
        .setxattr(f.ino.0, "user.davfs.do_free", b"1")
        .await
        .unwrap();
}

#[tokio::test]
async fn unknown_actions_are_unsupported_not_silently_accepted() {
    let h = harness().await;
    let err = h
        .vfs
        .setxattr(ROOT, "user.davfs.do_nonsense", b"1")
        .await
        .unwrap_err();
    assert_eq!(err.errno(), libc::EROFS); // Unsupported -> ENOTSUP at the shim
}

// ---------------------------------------------------------------------------
// Google Drive shape
// ---------------------------------------------------------------------------

/// Duplicate names must both be reachable, with stable distinct names.
#[tokio::test]
async fn duplicate_names_are_disambiguated_and_both_readable() {
    let mock = MockRemote::id_addressed();
    let a = mock.add_file("/", "report.pdf", b"first");
    let b = mock.add_file("/", "report.pdf", b"second");
    let h = harness_with(mock, VfsConfig::default()).await;

    let entries = h.vfs.readdir(ROOT).await.unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|(_, _, n)| n.as_str())
        .filter(|n| *n != "." && *n != "..")
        .collect();

    assert_eq!(names.len(), 2, "a duplicate name was lost: {names:?}");
    assert_ne!(names[0], names[1]);
    assert!(names.iter().all(|n| n.ends_with(".pdf")));

    // Both resolve, and to different content.
    let mut contents = Vec::new();
    for name in &names {
        let attr = h.vfs.lookup(ROOT, name).await.unwrap();
        let fh = h.vfs.open(attr.ino.0, false).await.unwrap();
        contents.push(h.vfs.read(fh, 0, 64).await.unwrap());
    }
    contents.sort();
    assert_eq!(contents, vec![b"first".to_vec(), b"second".to_vec()]);
    let _ = (a, b);
}

/// A size-less node must not report 0, or readers conclude it is empty.
#[tokio::test]
async fn export_only_nodes_report_a_placeholder_then_the_true_size() {
    let exported = b"the exported document body";
    let mock = MockRemote::id_addressed();
    mock.add_export_only("/", "Notes", exported);
    let h = harness_with(mock, VfsConfig::default()).await;

    let entries = h.vfs.readdir(ROOT).await.unwrap();
    let name = entries
        .iter()
        .map(|(_, _, n)| n.clone())
        .find(|n| n.starts_with("Notes"))
        .expect("the export-only node is listed");
    // The naming pipeline added the export extension.
    assert_eq!(name, "Notes.odt");

    let attr = h.vfs.lookup(ROOT, &name).await.unwrap();
    assert_ne!(attr.size, 0, "a size-less node reported 0 bytes");

    // After the content is fetched, the true length is reported.
    let fh = h.vfs.open(attr.ino.0, false).await.unwrap();
    assert_eq!(h.vfs.read(fh, 0, 1024).await.unwrap(), exported);
    let attr = h.vfs.getattr(attr.ino.0).await.unwrap();
    assert_eq!(attr.size, exported.len() as u64);
}

// ---------------------------------------------------------------------------
// Filesystem-wide
// ---------------------------------------------------------------------------

#[tokio::test]
async fn statfs_reports_something_usable() {
    // ENOSYS here makes `df` and several GTK file choosers misbehave.
    let h = harness().await;
    let s = h.vfs.statfs().await;
    assert!(s.blocks > 0);
    assert!(s.bfree <= s.blocks);
    assert_eq!(s.namelen, 255);
    assert!(s.bsize >= 512);
}

#[tokio::test]
async fn initial_sync_walks_breadth_first_within_its_depth_limit() {
    let mock = MockRemote::new();
    mock.add_dir("/", "a");
    mock.add_dir("/a", "b");
    mock.add_dir("/a/b", "c");
    mock.add_file("/a/b/c", "deep.txt", b"deep");
    let h = harness_with(mock, VfsConfig::default()).await;

    let listed = h.vfs.initial_sync(2).await.unwrap();
    assert!(listed >= 2, "expected at least root and /a, got {listed}");

    h.mock.go_offline();
    // Root and /a were cached, so they still list.
    assert!(h.vfs.readdir(ROOT).await.is_ok());
    let a = h.vfs.lookup(ROOT, "a").await.unwrap();
    assert!(h.vfs.readdir(a.ino.0).await.is_ok());
}

#[tokio::test]
async fn write_open_is_refused_on_a_read_only_mount() {
    let h = harness().await;
    let f = h.vfs.lookup(ROOT, "hello.txt").await.unwrap();
    let err = h.vfs.open(f.ino.0, true).await.unwrap_err();
    // EROFS, never ENOSYS: ENOSYS would make the kernel disable the op for the
    // lifetime of the mount.
    assert_eq!(err.errno(), libc::EROFS);
}
