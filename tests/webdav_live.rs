//! The WebDAV client against a real server.
//!
//! This is the acceptance test for the parser rewrite. Before it, every
//! assertion here failed: the client returned zero entries from `rclone serve
//! webdav`, because it split on the literal `<d:response>` and rclone emits
//! `<D:response>` -- on a single line.

mod common;

use common::{rclone::RcloneServer, tree};
use davfs_sync::webdav::{ListResult, WebDavClient};

/// rclone is required for these; skip cleanly on a machine without it, but
/// fail loudly in CI where DAVFS_E2E_REQUIRE is set.
macro_rules! require_rclone {
    () => {
        if !RcloneServer::available() {
            if std::env::var_os("DAVFS_E2E_REQUIRE").is_some() {
                panic!("rclone is required (DAVFS_E2E_REQUIRE is set) but not installed");
            }
            eprintln!("SKIP {}: rclone not installed", module_path!());
            return;
        }
    };
}

fn client(server: &RcloneServer) -> WebDavClient {
    WebDavClient::new(
        server.base.to_string(),
        common::rclone::USER.to_string(),
        common::rclone::PASS.to_string(),
    )
    .expect("client")
}

#[tokio::test]
async fn lists_a_real_server() {
    require_rclone!();
    let dir = tree::build().expect("fixture tree");
    let server = RcloneServer::start(dir.path()).expect("rclone");
    let c = client(&server);

    let ListResult::Modified { entries, .. } = c.list_dir("", None).await.expect("list root")
    else {
        panic!("a first listing cannot be NotModified: no validator was sent");
    };

    let mut got: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    got.sort_unstable();
    assert_eq!(
        got,
        tree::expected_root_names(),
        "listing did not round-trip. rclone stderr:\n{}",
        server.stderr()
    );
}

/// Percent-encoding and XML entities must survive the round trip.
#[tokio::test]
async fn hostile_names_survive() {
    require_rclone!();
    let dir = tree::build().expect("fixture tree");
    let server = RcloneServer::start(dir.path()).expect("rclone");
    let c = client(&server);

    let ListResult::Modified { entries, .. } = c.list_dir("", None).await.unwrap() else {
        panic!("unexpected NotModified");
    };
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

    for want in [
        "hash#and&.txt",    // '#' truncated the URL under Url::join
        "apostrophe's.txt", // arrived as "apostrophe&#39;s.txt" unescaped
        "100%done.txt",     // a literal percent, not an escape
        "spaces in name.txt",
        "ümlaut-Ä.txt",
        "emoji-🚀.md",
    ] {
        assert!(names.contains(&want), "missing {want:?}; got {names:?}");
    }
}

/// A directory must not contain itself.
#[tokio::test]
async fn directories_do_not_list_themselves() {
    require_rclone!();
    let dir = tree::build().expect("fixture tree");
    let server = RcloneServer::start(dir.path()).expect("rclone");
    let c = client(&server);

    // rclone answers PROPFIND /nested/a/b/c/ with a self-entry whose
    // displayname is "c". The old parser added it as a child, so `ls` showed a
    // `c` directory inside `c`, recursively.
    let ListResult::Modified { entries, .. } = c.list_dir("nested/a/b/c", None).await.unwrap()
    else {
        panic!("unexpected NotModified");
    };

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["deep.txt"], "self-entry leaked into children");
}

#[tokio::test]
async fn nested_directories_and_sizes_are_correct() {
    require_rclone!();
    let dir = tree::build().expect("fixture tree");
    let server = RcloneServer::start(dir.path()).expect("rclone");
    let c = client(&server);

    let ListResult::Modified { entries, .. } = c.list_dir("nested/a/b", None).await.unwrap() else {
        panic!("unexpected NotModified");
    };
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "c");
    assert!(entries[0].is_dir);

    let ListResult::Modified { entries, .. } = c.list_dir("", None).await.unwrap() else {
        panic!("unexpected NotModified");
    };
    let large = entries.iter().find(|e| e.name == "large.bin").unwrap();
    assert_eq!(large.size, tree::LARGE_LEN as u64);
    assert!(!large.is_dir);

    let empty = entries.iter().find(|e| e.name == "empty").unwrap();
    assert!(empty.is_dir);
}

/// The mtime the PoC parsed, stored, and then threw away.
#[tokio::test]
async fn mtimes_are_reported() {
    require_rclone!();
    let dir = tree::build().expect("fixture tree");
    let server = RcloneServer::start(dir.path()).expect("rclone");
    let c = client(&server);

    let ListResult::Modified { entries, .. } = c.list_dir("", None).await.unwrap() else {
        panic!("unexpected NotModified");
    };
    let pinned = entries
        .iter()
        .find(|e| e.name == tree::KNOWN_MTIME_FILE)
        .expect("pinned file");

    let mtime = pinned.modified.expect("mtime present").timestamp();
    // HTTP dates have second granularity.
    assert!(
        (mtime - tree::KNOWN_MTIME_UNIX).abs() <= 1,
        "mtime {mtime} != expected {}",
        tree::KNOWN_MTIME_UNIX
    );
}

#[tokio::test]
async fn an_empty_directory_lists_as_empty_not_as_an_error() {
    require_rclone!();
    let dir = tree::build().expect("fixture tree");
    let server = RcloneServer::start(dir.path()).expect("rclone");
    let c = client(&server);

    let ListResult::Modified { entries, .. } = c.list_dir("empty", None).await.unwrap() else {
        panic!("unexpected NotModified");
    };
    assert!(entries.is_empty());
}

#[tokio::test]
async fn file_content_downloads_intact() {
    require_rclone!();
    let dir = tree::build().expect("fixture tree");
    let server = RcloneServer::start(dir.path()).expect("rclone");
    let c = client(&server);

    assert_eq!(c.read_file("plain.txt").await.unwrap(), b"hello\n");
    // A name that url_for must percent-encode correctly.
    assert_eq!(c.read_file("hash#and&.txt").await.unwrap(), b"hash\n");
    assert_eq!(
        c.read_file("nested/a/b/c/deep.txt").await.unwrap(),
        b"deep\n"
    );
    assert_eq!(
        c.read_file("large.bin").await.unwrap(),
        tree::large_content()
    );
}

#[tokio::test]
async fn missing_paths_report_not_found_not_a_generic_error() {
    require_rclone!();
    let dir = tree::build().expect("fixture tree");
    let server = RcloneServer::start(dir.path()).expect("rclone");
    let c = client(&server);

    let err = c.list_dir("does/not/exist", None).await.unwrap_err();
    assert_eq!(err.errno(), libc::ENOENT, "got {err:?}");
}

#[tokio::test]
async fn bad_credentials_are_unauthorized_and_not_retryable() {
    require_rclone!();
    let dir = tree::build().expect("fixture tree");
    let server = RcloneServer::start(dir.path()).expect("rclone");

    let c = WebDavClient::new(server.base.to_string(), "wrong".into(), "wrong".into()).unwrap();
    let err = c.list_dir("", None).await.unwrap_err();

    assert_eq!(err.errno(), libc::EACCES, "got {err:?}");
    // Retrying a 401 is what trips Nextcloud's brute-force protection.
    assert!(!err.is_retryable());
}

/// A server that is not there must be Offline (ENETUNREACH), never ENOENT.
#[tokio::test]
async fn an_unreachable_server_is_offline_not_missing() {
    // Port 1 on loopback: nothing listens there.
    let c = WebDavClient::with_timeout(
        "http://127.0.0.1:1/".into(),
        "u".into(),
        "p".into(),
        std::time::Duration::from_secs(2),
    )
    .unwrap();

    let err = c.list_dir("", None).await.unwrap_err();
    assert_eq!(
        err.errno(),
        libc::ENETUNREACH,
        "an unreachable server must not look like a missing file: {err:?}"
    );
    assert!(err.is_retryable());
}
