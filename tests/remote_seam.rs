//! The seam, exercised through both implementations.
//!
//! Every test here runs against `WebDavBackend` (over a real rclone server) and
//! `MockRemote` in its id-addressed, duplicate-name, cursor-feed configuration
//! -- the Google Drive shape. A test that passes only against WebDAV means the
//! seam has quietly become WebDAV-specific.

mod common;

use common::{rclone::RcloneServer, tree};
use davfs_sync::remote::{
    mock::MockRemote, ByteRange, ChangeFeed, Listing, NodeId, RemoteStorage, SizeHint, Validator,
    WebDavBackend,
};
use davfs_sync::webdav::WebDavClient;
use futures::StreamExt;
use std::sync::Arc;

/// Collect a ContentStream into bytes.
async fn drain(mut s: davfs_sync::remote::ContentStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = s.body.next().await {
        out.extend_from_slice(&chunk.expect("chunk"));
    }
    out
}

fn mock_backend() -> Arc<dyn RemoteStorage> {
    let m = MockRemote::id_addressed();
    let dir = m.add_dir("/", "sub");
    m.add_file("/", "hello.txt", b"hello\n");
    m.add_file(dir.as_str(), "inner.txt", b"inner\n");
    Arc::new(m)
}

// ---------------------------------------------------------------------------
// Trait-level behaviour, asserted against the mock (both shapes)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn listing_separates_the_directory_from_its_children() {
    let backend = mock_backend();
    let Listing::Fresh {
        this,
        children,
        complete,
        ..
    } = backend.list(&backend.root(), None).await.unwrap()
    else {
        panic!("first listing cannot be Unchanged");
    };

    assert!(complete);
    assert!(
        this.is_some(),
        "the directory's own entry must be available"
    );
    let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"hello.txt"));
    assert!(names.contains(&"sub"));
    assert!(
        !names.contains(&""),
        "the self-entry leaked into the children"
    );
}

#[tokio::test]
async fn unchanged_is_distinct_from_empty() {
    let m = MockRemote::new();
    let backend: Arc<dyn RemoteStorage> = {
        m.add_file("/", "a.txt", b"a");
        m.set_dir_validator(&NodeId("/".into()), Validator::Etag("\"v1\"".into()));
        Arc::new(m)
    };

    // Matching validator -> Unchanged.
    let r = backend
        .list(&backend.root(), Some(&Validator::Etag("\"v1\"".into())))
        .await
        .unwrap();
    assert_eq!(r, Listing::Unchanged);

    // Stale validator -> a fresh listing.
    let r = backend
        .list(&backend.root(), Some(&Validator::Etag("\"old\"".into())))
        .await
        .unwrap();
    assert!(matches!(r, Listing::Fresh { .. }));

    // An empty directory is Fresh-with-no-children, never Unchanged.
    let empty = MockRemote::new();
    empty.add_dir("/", "empty");
    let backend: Arc<dyn RemoteStorage> = Arc::new(empty);
    let Listing::Fresh { children, .. } =
        backend.list(&NodeId("/empty".into()), None).await.unwrap()
    else {
        panic!("an empty directory must not report Unchanged");
    };
    assert!(children.is_empty());
}

/// The id-addressed shape must not be derivable from the path.
#[tokio::test]
async fn id_addressed_backends_refuse_to_synthesize_child_ids() {
    let backend = mock_backend();
    assert!(!backend.capabilities().path_addressed);
    assert_eq!(
        backend.child_id(&backend.root(), "hello.txt"),
        None,
        "an id-addressed backend must not invent ids from names"
    );

    // Resolution therefore has to go through lookup().
    let entry = backend.lookup(&backend.root(), "hello.txt").await.unwrap();
    assert_eq!(entry.name, "hello.txt");
    assert!(
        !entry.id.as_str().contains("hello.txt"),
        "the id should be opaque, not path-derived: {}",
        entry.id
    );
}

#[tokio::test]
async fn duplicate_names_are_representable() {
    // Drive allows this; the seam must not lose one of them.
    let m = MockRemote::id_addressed();
    let a = m.add_file("/", "report.pdf", b"one");
    let b = m.add_file("/", "report.pdf", b"two");
    assert_ne!(a, b, "duplicate names must still get distinct ids");

    let backend: Arc<dyn RemoteStorage> = Arc::new(m);
    assert!(!backend.capabilities().unique_names);

    let Listing::Fresh { children, .. } = backend.list(&backend.root(), None).await.unwrap() else {
        panic!()
    };
    let dupes = children.iter().filter(|c| c.name == "report.pdf").count();
    assert_eq!(dupes, 2, "a duplicate name was dropped");
}

#[tokio::test]
async fn export_only_nodes_report_unknown_size_not_zero() {
    let m = MockRemote::id_addressed();
    let id = m.add_export_only("/", "Notes", b"exported content");
    let backend: Arc<dyn RemoteStorage> = Arc::new(m);

    let entry = backend.stat(&id).await.unwrap();
    assert_eq!(entry.size, SizeHint::Unknown);
    assert_ne!(
        entry.size.or_placeholder(),
        0,
        "reporting 0 is what makes Google Docs unreadable through a mount"
    );

    // The content is still readable.
    let stream = backend.open_read(&id, None, None).await.unwrap();
    assert_eq!(drain(stream).await, b"exported content");
}

#[tokio::test]
async fn a_changed_file_fails_a_conditional_read_rather_than_splicing() {
    let m = MockRemote::new();
    let id = m.add_file("/", "f.txt", b"original");
    let stale = m.stat(&id).await.unwrap().validator.unwrap();

    m.set_content(&id, b"replaced");

    let backend: Arc<dyn RemoteStorage> = Arc::new(m);
    let err = backend
        .open_read(&id, None, Some(&stale))
        .await
        .unwrap_err();

    // ESTALE, so the kernel drops its handle and retries -- rather than two
    // versions of the file being stitched together.
    assert_eq!(err.errno(), libc::ESTALE, "got {err:?}");
}

#[tokio::test]
async fn cursor_backends_deliver_deltas_and_path_backends_refuse() {
    let m = MockRemote::id_addressed();
    let backend: Arc<dyn RemoteStorage> = Arc::new(m);
    assert_eq!(backend.change_feed(), ChangeFeed::Cursor);

    let cursor = backend.start_cursor().await.unwrap();
    let batch = backend.poll_changes(&cursor).await.unwrap();
    assert!(batch.changes.is_empty(), "nothing has happened yet");

    // A per-directory backend must say so rather than pretend.
    let path_based: Arc<dyn RemoteStorage> = Arc::new(MockRemote::new());
    assert_eq!(path_based.change_feed(), ChangeFeed::PerDirectory);
    let err = path_based.poll_changes("0").await.unwrap_err();
    assert_eq!(err.errno(), libc::EROFS); // Unsupported
}

#[tokio::test]
async fn offline_is_reported_as_offline_by_every_operation() {
    let m = MockRemote::new();
    m.add_file("/", "a.txt", b"a");
    m.go_offline();
    let backend: Arc<dyn RemoteStorage> = Arc::new(m);

    for errno in [
        backend.probe().await.unwrap_err().errno(),
        backend
            .list(&backend.root(), None)
            .await
            .unwrap_err()
            .errno(),
        backend
            .open_read(&NodeId("/a.txt".into()), None, None)
            .await
            .unwrap_err()
            .errno(),
    ] {
        assert_eq!(errno, libc::ENETUNREACH);
        assert_ne!(errno, libc::ENOENT, "offline must never look like missing");
    }
}

#[tokio::test]
async fn ranged_reads_return_the_requested_slice() {
    let m = MockRemote::new();
    let id = m.add_file("/", "f.bin", b"0123456789");
    let backend: Arc<dyn RemoteStorage> = Arc::new(m);

    let s = backend
        .open_read(
            &id,
            Some(ByteRange {
                offset: 3,
                len: Some(4),
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(s.total_size, Some(10), "total is the file, not the slice");
    assert_eq!(drain(s).await, b"3456");

    // Open-ended range.
    let s = backend
        .open_read(
            &id,
            Some(ByteRange {
                offset: 7,
                len: None,
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(drain(s).await, b"789");
}

// ---------------------------------------------------------------------------
// The same expectations against the real WebDAV backend
// ---------------------------------------------------------------------------

macro_rules! require_rclone {
    () => {
        if !RcloneServer::available() {
            if std::env::var_os("DAVFS_E2E_REQUIRE").is_some() {
                panic!("rclone required but missing");
            }
            eprintln!("SKIP {}: rclone not installed", module_path!());
            return;
        }
    };
}

fn dav_backend(server: &RcloneServer) -> WebDavBackend {
    WebDavBackend::new(
        WebDavClient::new(
            server.base.to_string(),
            common::rclone::USER.into(),
            common::rclone::PASS.into(),
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn webdav_reports_the_path_addressed_shape() {
    require_rclone!();
    let dir = tree::build().unwrap();
    let server = RcloneServer::start(dir.path()).unwrap();
    let backend = dav_backend(&server);

    let caps = backend.capabilities();
    assert!(caps.path_addressed);
    assert!(caps.unique_names);
    assert_eq!(backend.change_feed(), ChangeFeed::PerDirectory);
    assert_eq!(backend.root(), NodeId("/".into()));

    // Path addressing means a child id needs no round trip.
    assert_eq!(
        backend.child_id(&NodeId("/".into()), "plain.txt"),
        Some(NodeId("/plain.txt".into()))
    );
    assert_eq!(
        backend.child_id(&NodeId("/nested/a".into()), "b"),
        Some(NodeId("/nested/a/b".into()))
    );
}

#[tokio::test]
async fn webdav_listing_through_the_trait() {
    require_rclone!();
    let dir = tree::build().unwrap();
    let server = RcloneServer::start(dir.path()).unwrap();
    let backend: Arc<dyn RemoteStorage> = Arc::new(dav_backend(&server));

    let Listing::Fresh {
        this,
        children,
        complete,
        ..
    } = backend.list(&backend.root(), None).await.unwrap()
    else {
        panic!("unexpected Unchanged");
    };
    assert!(complete);
    assert!(this.is_some());

    let mut names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, tree::expected_root_names());

    // Ids are paths, and every entry knows its parent.
    let plain = children.iter().find(|c| c.name == "plain.txt").unwrap();
    assert_eq!(plain.id, NodeId("/plain.txt".into()));
    assert_eq!(plain.parent, Some(NodeId("/".into())));
    assert_eq!(plain.size, SizeHint::Exact(6));
}

#[tokio::test]
async fn webdav_subdirectory_does_not_contain_itself_through_the_trait() {
    require_rclone!();
    let dir = tree::build().unwrap();
    let server = RcloneServer::start(dir.path()).unwrap();
    let backend: Arc<dyn RemoteStorage> = Arc::new(dav_backend(&server));

    let Listing::Fresh { children, this, .. } = backend
        .list(&NodeId("/nested/a/b/c".into()), None)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(this.unwrap().id, NodeId("/nested/a/b/c".into()));
    let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["deep.txt"]);
}

#[tokio::test]
async fn webdav_lookup_and_stat() {
    require_rclone!();
    let dir = tree::build().unwrap();
    let server = RcloneServer::start(dir.path()).unwrap();
    let backend: Arc<dyn RemoteStorage> = Arc::new(dav_backend(&server));

    let e = backend.lookup(&backend.root(), "plain.txt").await.unwrap();
    assert_eq!(e.size, SizeHint::Exact(6));

    let e = backend.stat(&NodeId("/plain.txt".into())).await.unwrap();
    assert_eq!(e.name, "plain.txt");

    // The root is describable without a parent to scan.
    let root = backend.stat(&backend.root()).await.unwrap();
    assert!(root.kind.is_dir());
    assert_eq!(root.parent, None);

    let err = backend
        .lookup(&backend.root(), "nope.txt")
        .await
        .unwrap_err();
    assert_eq!(err.errno(), libc::ENOENT);
}

#[tokio::test]
async fn webdav_streams_content_whole_and_ranged() {
    require_rclone!();
    let dir = tree::build().unwrap();
    let server = RcloneServer::start(dir.path()).unwrap();
    let backend: Arc<dyn RemoteStorage> = Arc::new(dav_backend(&server));

    let s = backend
        .open_read(&NodeId("/plain.txt".into()), None, None)
        .await
        .unwrap();
    assert_eq!(drain(s).await, b"hello\n");

    // A name that must be percent-encoded on the way out.
    let s = backend
        .open_read(&NodeId("/hash#and&.txt".into()), None, None)
        .await
        .unwrap();
    assert_eq!(drain(s).await, b"hash\n");

    // Ranged: rclone supports Range, so this exercises the real header path.
    let s = backend
        .open_read(
            &NodeId("/large.bin".into()),
            Some(ByteRange {
                offset: 1000,
                len: Some(256),
            }),
            None,
        )
        .await
        .unwrap();
    let got = drain(s).await;
    assert_eq!(got.len(), 256);
    assert_eq!(got, &tree::large_content()[1000..1256]);
}

/// The whole file, streamed, must equal the whole file.
#[tokio::test]
async fn webdav_streams_a_large_file_intact() {
    require_rclone!();
    let dir = tree::build().unwrap();
    let server = RcloneServer::start(dir.path()).unwrap();
    let backend: Arc<dyn RemoteStorage> = Arc::new(dav_backend(&server));

    let s = backend
        .open_read(&NodeId("/large.bin".into()), None, None)
        .await
        .unwrap();
    assert_eq!(s.total_size, Some(tree::LARGE_LEN as u64));
    assert_eq!(drain(s).await, tree::large_content());
}

/// The write half is implemented and works, but nothing above calls it yet.
#[tokio::test]
async fn webdav_write_half_round_trips() {
    require_rclone!();
    let dir = tree::build().unwrap();
    let server = RcloneServer::start(dir.path()).unwrap();
    let backend: Arc<dyn RemoteStorage> = Arc::new(dav_backend(&server));

    let made = backend.create_dir(&backend.root(), "newdir").await.unwrap();
    assert_eq!(made.id, NodeId("/newdir".into()));
    assert!(dir.path().join("newdir").is_dir());

    let file = backend
        .write_content(
            &NodeId("/newdir/w.txt".into()),
            davfs_sync::remote::ContentSource::Bytes(bytes::Bytes::from_static(b"written")),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("newdir/w.txt")).unwrap(),
        b"written"
    );

    backend.remove(&file.id, None).await.unwrap();
    assert!(!dir.path().join("newdir/w.txt").exists());
}

/// What conditional writes actually do, on this server.
///
/// The plan called for proving `if_unchanged` against a real 412 before
/// building a conflict policy on it. The answer, verified with curl as well as
/// here: **rclone's WebDAV server ignores conditional-write headers entirely**
/// -- `If-Match` on PUT, `If-None-Match: *` on an existing file, and `If-Match`
/// on DELETE all return 201/204 and perform the operation.
///
/// Nextcloud and other SabreDAV servers do implement RFC 4918 `If` handling, so
/// this is per-server rather than universal. The test therefore asserts the
/// invariant that holds either way, and records which branch was taken. The
/// consequence for the eventual conflict policy: a precondition is necessary
/// but not sufficient, and correctness-critical writes must re-read and compare.
#[tokio::test]
async fn webdav_conditional_write_behaviour_is_recorded() {
    require_rclone!();
    let dir = tree::build().unwrap();
    let server = RcloneServer::start(dir.path()).unwrap();
    let backend: Arc<dyn RemoteStorage> = Arc::new(dav_backend(&server));

    let target = NodeId("/plain.txt".into());
    let stale = Validator::Etag("\"definitely-not-current\"".into());

    let result = backend
        .write_content(
            &target,
            davfs_sync::remote::ContentSource::Bytes(bytes::Bytes::from_static(b"clobbered")),
            Some(&stale),
        )
        .await;

    let on_disk = std::fs::read(dir.path().join("plain.txt")).unwrap();

    match result {
        Err(e) => {
            // A compliant server: 412 -> Changed -> ESTALE, file untouched.
            assert_eq!(e.errno(), libc::ESTALE, "expected a 412, got {e:?}");
            assert_eq!(
                on_disk, b"hello\n",
                "the precondition was rejected but the file changed anyway"
            );
        }
        Ok(_) => {
            // rclone: the precondition was ignored and the write went through.
            assert_eq!(
                on_disk, b"clobbered",
                "the write reported success without taking effect"
            );
            eprintln!(
                "NOTE: this server ignores conditional-write preconditions; \
                 a conflict policy cannot rely on If-Match alone here"
            );
        }
    }
}

/// The mock enforces preconditions, so the compliant-server path is covered
/// regardless of which WebDAV server the suite happens to run against.
#[tokio::test]
async fn a_compliant_backend_rejects_a_stale_conditional_write() {
    let m = davfs_sync::remote::mock::MockRemote::new();
    let id = m.add_file("/", "f.txt", b"original");
    let stale = Validator::Etag("\"stale\"".into());
    let backend: Arc<dyn RemoteStorage> = Arc::new(m);

    assert!(backend.capabilities().enforces_preconditions);

    let err = backend
        .write_content(
            &id,
            davfs_sync::remote::ContentSource::Bytes(bytes::Bytes::from_static(b"clobbered")),
            Some(&stale),
        )
        .await
        .unwrap_err();
    assert_eq!(err.errno(), libc::ESTALE);

    // ...and the content is untouched.
    let s = backend.open_read(&id, None, None).await.unwrap();
    assert_eq!(drain(s).await, b"original");
}

#[tokio::test]
async fn webdav_probe_distinguishes_auth_from_offline() {
    require_rclone!();
    let dir = tree::build().unwrap();
    let server = RcloneServer::start(dir.path()).unwrap();

    let good = dav_backend(&server);
    good.probe().await.unwrap();

    let bad = WebDavBackend::new(
        WebDavClient::new(server.base.to_string(), "no".into(), "no".into()).unwrap(),
    );
    assert_eq!(bad.probe().await.unwrap_err().errno(), libc::EACCES);
}
