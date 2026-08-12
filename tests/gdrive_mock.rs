//! The Google Drive backend against a mock HTTP server.
//!
//! There is no Drive emulator, and a service account cannot hold files (0 GB
//! quota since 2023-06-01), so this is the only tier that can run in CI. See
//! issue #1 for what it would take to test against real Drive.
//!
//! What this tier is *for*: the HTTP and JSON mapping, the OAuth refresh
//! behaviour, and error classification. The Drive-shaped *model* -- opaque ids,
//! duplicate names, unknown sizes, the cursor feed -- is covered independently
//! by `MockRemote::id_addressed()` in `tests/remote_seam.rs`, which runs the
//! same suite as WebDAV.

use std::sync::Arc;
use std::time::Duration;

use davfs_sync::remote::gdrive::{
    auth::{OAuthClient, TokenSource},
    pacer::Pacer,
    GDriveBackend, GDriveConfig,
};
use davfs_sync::remote::{
    ByteRange, Change, ChangeFeed, Listing, NodeId, NodeKind, RemoteStorage, SizeHint, Validator,
};
use futures::StreamExt;
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The `fields=` projection the client sends, mirrored here so fixtures and
/// code cannot drift apart silently.
const FILE_FIELDS: &str = davfs_sync::remote::gdrive::api::FILE_FIELDS;

/// Stand up a token endpoint that always succeeds, so tests can focus on Drive.
async fn with_token(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "test-access-token",
            "expires_in": 3599,
            "token_type": "Bearer"
        })))
        .mount(server)
        .await;
}

fn backend(server: &MockServer) -> GDriveBackend {
    backend_with(server, GDriveConfig::default())
}

fn backend_with(server: &MockServer, mut config: GDriveConfig) -> GDriveBackend {
    config.api_base = server.uri();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let oauth = OAuthClient::new("test-client", "test-secret")
        .with_token_endpoint(format!("{}/token", server.uri()));
    let tokens = Arc::new(TokenSource::new(http.clone(), oauth, "test-refresh-token"));
    // No artificial delay: the pacer has its own unit tests.
    GDriveBackend::new(http, tokens, config).with_pacer(Pacer::unlimited())
}

fn fixture(name: &str) -> serde_json::Value {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/gdrive")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
}

async fn drain(mut s: davfs_sync::remote::ContentStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = s.body.next().await {
        out.extend_from_slice(&chunk.expect("chunk"));
    }
    out
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lists_a_folder_and_maps_every_kind() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("files_list_root.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/root"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "root", "name": "My Drive",
            "mimeType": "application/vnd.google-apps.folder"
        })))
        .mount(&server)
        .await;

    let b = backend(&server);
    let Listing::Fresh {
        this,
        children,
        validator,
        complete,
    } = b.list(&b.root(), None).await.expect("list")
    else {
        panic!("Drive has no per-folder validator, so it cannot report Unchanged");
    };

    assert!(complete);
    assert!(this.is_some());
    assert_eq!(
        validator, None,
        "Drive has no per-folder ETag; freshness comes from the change feed"
    );

    let by_name = |n: &str| children.iter().find(|c| c.name == n).cloned();

    // A binary file: exact size, md5 validator.
    let photo = by_name("photo.jpg").expect("photo.jpg");
    assert_eq!(photo.kind, NodeKind::File);
    assert_eq!(photo.size, SizeHint::Exact(1_048_576));
    assert!(matches!(photo.validator, Some(Validator::Checksum { .. })));

    // A folder.
    let docs = by_name("Documents").expect("Documents");
    assert_eq!(docs.kind, NodeKind::Dir);

    // A Google-native document: no size at all.
    let notes = by_name("Meeting Notes").expect("Meeting Notes");
    assert_eq!(notes.kind, NodeKind::ExportOnly);
    assert_eq!(notes.size, SizeHint::Unknown);
    assert_ne!(
        notes.size.or_placeholder(),
        0,
        "reporting 0 is what makes Google Docs unreadable through a mount"
    );

    // A shortcut keeps its target.
    let link = by_name("shortcut to photo").expect("shortcut");
    assert!(link.flags.shortcut);
    assert_eq!(link.target, Some(NodeId("1photoID".into())));
}

/// Two files with the same name in one folder must both survive.
#[tokio::test]
async fn duplicate_names_are_both_returned() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("files_list_duplicates.json")),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/root"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let b = backend(&server);
    let Listing::Fresh { children, .. } = b.list(&b.root(), None).await.unwrap() else {
        panic!()
    };

    let dupes: Vec<_> = children.iter().filter(|c| c.name == "report.pdf").collect();
    assert_eq!(dupes.len(), 2, "a duplicate name was dropped");
    assert_ne!(
        dupes[0].id, dupes[1].id,
        "duplicates must have distinct ids"
    );
}

/// A folder larger than one page must be fetched completely, or the cache would
/// delete everything past the first page.
#[tokio::test]
async fn pagination_is_followed_to_the_end() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(query_param("pageToken", "page-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "files": [{"id": "b", "name": "second.txt", "mimeType": "text/plain",
                       "parents": ["root"], "size": "2", "version": "1"}]
        })))
        .mount(&server)
        .await;
    // The first page is whatever does not carry a pageToken.
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "files": [{"id": "a", "name": "first.txt", "mimeType": "text/plain",
                       "parents": ["root"], "size": "1", "version": "1"}],
            "nextPageToken": "page-2"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/root"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let b = backend(&server);
    let Listing::Fresh {
        children, complete, ..
    } = b.list(&b.root(), None).await.unwrap()
    else {
        panic!()
    };

    let mut names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["first.txt", "second.txt"],
        "pagination stopped early"
    );
    assert!(complete, "a fully-paged listing must be marked complete");
}

/// Trashed files are still returned by the API unless excluded.
#[tokio::test]
async fn the_query_excludes_trashed_files_and_supports_shared_drives() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(query_param("q", "'root' in parents and trashed = false"))
        .and(query_param("supportsAllDrives", "true"))
        .and(query_param("includeItemsFromAllDrives", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"files": []})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/root"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let b = backend(&server);
    // The mock only matches when every one of those parameters is present.
    assert!(b.list(&b.root(), None).await.is_ok());
}

// ---------------------------------------------------------------------------
// Content
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_binary_file_downloads_with_alt_media() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/files/1photoID"))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"binary-content".to_vec()))
        .mount(&server)
        .await;
    // stat, which open_read does first to learn the kind.
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/1photoID"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "1photoID", "name": "photo.jpg", "mimeType": "image/jpeg",
            "parents": ["root"], "size": "14", "md5Checksum": "abc", "version": "2"
        })))
        .mount(&server)
        .await;

    let b = backend(&server);
    let stream = b
        .open_read(&NodeId("1photoID".into()), None, None)
        .await
        .expect("open_read");
    assert_eq!(drain(stream).await, b"binary-content");
}

/// A Google-native document has to go through `files.export`, and export does
/// not honour `Range`.
#[tokio::test]
async fn a_native_document_is_exported_rather_than_downloaded() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/files/1docID/export"))
        .and(query_param(
            "mimeType",
            "application/vnd.oasis.opendocument.text",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"exported-odt-bytes".to_vec()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/1docID"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "1docID", "name": "Meeting Notes",
            "mimeType": "application/vnd.google-apps.document",
            "parents": ["root"], "version": "5"
        })))
        .mount(&server)
        .await;

    let b = backend(&server);
    // A range is requested, and must be ignored for an export.
    let stream = b
        .open_read(
            &NodeId("1docID".into()),
            Some(ByteRange {
                offset: 4,
                len: Some(4),
            }),
            None,
        )
        .await
        .expect("export");
    assert_eq!(
        drain(stream).await,
        b"exported-odt-bytes",
        "a ranged request must not be sent for an export"
    );
}

/// Forms, Sites and Maps have no export at all. Inventing one would hand the
/// user a file that cannot be opened.
#[tokio::test]
async fn a_document_type_with_no_export_is_rejected_clearly() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/files/1formID"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "1formID", "name": "Survey",
            "mimeType": "application/vnd.google-apps.form",
            "parents": ["root"], "version": "1"
        })))
        .mount(&server)
        .await;

    let b = backend(&server);
    let err = b
        .open_read(&NodeId("1formID".into()), None, None)
        .await
        .unwrap_err();
    // EROFS via Unsupported: a clear "cannot", not a spurious I/O error.
    assert_eq!(err.errno(), libc::EROFS, "got {err:?}");
}

#[tokio::test]
async fn a_ranged_read_sends_a_range_header() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/files/1binID"))
        .and(query_param("alt", "media"))
        .and(wiremock::matchers::header("range", "bytes=100-199"))
        .respond_with(
            ResponseTemplate::new(206)
                .set_body_bytes(vec![7u8; 100])
                .insert_header("content-range", "bytes 100-199/4096"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/1binID"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "1binID", "name": "big.bin", "mimeType": "application/octet-stream",
            "parents": ["root"], "size": "4096", "version": "1"
        })))
        .mount(&server)
        .await;

    let b = backend(&server);
    let stream = b
        .open_read(
            &NodeId("1binID".into()),
            Some(ByteRange {
                offset: 100,
                len: Some(100),
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        stream.total_size,
        Some(4096),
        "total size must come from Content-Range, not the slice length"
    );
    assert_eq!(drain(stream).await.len(), 100);
}

// ---------------------------------------------------------------------------
// The change feed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_start_cursor_is_fetched() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/changes/startPageToken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("start_page_token.json")))
        .mount(&server)
        .await;

    let b = backend(&server);
    assert_eq!(b.start_cursor().await.unwrap(), "12345");
}

#[tokio::test]
async fn changes_map_to_upserts_and_removals() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/changes"))
        .and(query_param("includeRemoved", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("changes_page.json")))
        .mount(&server)
        .await;

    let b = backend(&server);
    assert_eq!(b.change_feed(), ChangeFeed::Cursor);

    let batch = b.poll_changes("100").await.unwrap();
    assert!(!batch.more, "a page with newStartPageToken is the last one");
    assert_eq!(batch.cursor, "200");

    let upserted: Vec<_> = batch
        .changes
        .iter()
        .filter_map(|c| match c {
            Change::Upserted(e) => Some(e.name.clone()),
            _ => None,
        })
        .collect();
    let removed: Vec<_> = batch
        .changes
        .iter()
        .filter_map(|c| match c {
            Change::Removed(id) => Some(id.0.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(upserted, vec!["updated.txt"]);
    // Both an explicit removal and a file moved to the trash: from the mount's
    // point of view a trashed file is gone.
    assert!(removed.contains(&"deletedFileId".to_string()));
    assert!(
        removed.contains(&"trashedFileId".to_string()),
        "a trashed file must be treated as removed, not as a live file"
    );
}

#[tokio::test]
async fn more_is_reported_while_pages_remain() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/changes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "changes": [],
            "nextPageToken": "next-page"
        })))
        .mount(&server)
        .await;

    let b = backend(&server);
    let batch = b.poll_changes("1").await.unwrap();
    assert!(batch.more);
    assert_eq!(batch.cursor, "next-page");
}

/// An aged-out page token breaks the delta stream; the caller must resync.
#[tokio::test]
async fn an_expired_page_token_is_distinguishable() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/changes"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"code": 400, "message": "Invalid Value",
                      "errors": [{"reason": "invalidPageToken"}]}
        })))
        .mount(&server)
        .await;

    let b = backend(&server);
    let err = b.poll_changes("stale").await.unwrap_err();
    assert_eq!(
        err.errno(),
        libc::ESTALE,
        "an expired cursor must be distinguishable from any other 400: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Errors and auth
// ---------------------------------------------------------------------------

/// Both arrive as HTTP 403, and confusing them makes a busy mount look broken.
#[tokio::test]
async fn a_rate_limit_is_retried_but_a_permission_failure_is_not() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/files/rateLimited"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": {"code": 403, "errors": [{"reason": "userRateLimitExceeded"}]}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/forbidden"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": {"code": 403, "errors": [{"reason": "insufficientFilePermissions"}]}
        })))
        .mount(&server)
        .await;

    // One attempt, so the test does not sit through backoff.
    let b = backend_with(
        &server,
        GDriveConfig {
            retry: davfs_sync::remote::RetryPolicy::none(),
            ..Default::default()
        },
    );

    let rate = b.stat(&NodeId("rateLimited".into())).await.unwrap_err();
    assert!(rate.is_retryable(), "a rate limit must be retryable");
    assert_eq!(rate.errno(), libc::EAGAIN);

    let perm = b.stat(&NodeId("forbidden".into())).await.unwrap_err();
    assert!(
        !perm.is_retryable(),
        "retrying a permission failure is futile"
    );
    assert_eq!(perm.errno(), libc::EACCES);
}

#[tokio::test]
async fn a_missing_file_is_enoent() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/files/nope"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": {"code": 404, "message": "File not found: nope."}
        })))
        .mount(&server)
        .await;

    let b = backend(&server);
    assert_eq!(
        b.stat(&NodeId("nope".into())).await.unwrap_err().errno(),
        libc::ENOENT
    );
}

/// An access token can be revoked before its nominal expiry; one
/// refresh-and-retry is the documented remedy.
#[tokio::test]
async fn a_401_triggers_exactly_one_refresh_and_retry() {
    let server = MockServer::start().await;
    with_token(&server).await;

    // Fails once with 401, then succeeds. `up_to_n_times` makes the ordering
    // explicit rather than relying on mock precedence.
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/retried"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {"code": 401, "message": "Invalid Credentials"}
        })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/retried"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "retried", "name": "ok.txt", "mimeType": "text/plain",
            "parents": ["root"], "size": "2", "version": "1"
        })))
        .mount(&server)
        .await;

    let b = backend(&server);
    let entry = b
        .stat(&NodeId("retried".into()))
        .await
        .expect("retry after 401");
    assert_eq!(entry.name, "ok.txt");
}

/// A dead refresh token cannot be fixed by retrying; only re-consent helps.
#[tokio::test]
async fn a_dead_refresh_token_reports_auth_expired() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_grant",
            "error_description": "Token has been expired or revoked."
        })))
        .mount(&server)
        .await;

    let b = backend(&server);
    let err = b.probe().await.unwrap_err();

    assert_eq!(err.errno(), libc::EACCES, "got {err:?}");
    assert!(
        !err.is_retryable(),
        "a revoked refresh token cannot be recovered by retrying; \
         the 7-day expiry on a Testing-status app lands here"
    );
}

/// Twenty concurrent requests must produce one token refresh, not twenty.
/// Google allows only 100 refresh tokens per account per client, and exceeding
/// that silently invalidates the oldest.
#[tokio::test]
async fn concurrent_requests_refresh_the_token_only_once() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "tok", "expires_in": 3599
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/x"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "x", "name": "x", "mimeType": "text/plain", "version": "1"
        })))
        .mount(&server)
        .await;

    let b = Arc::new(backend(&server));
    let mut handles = Vec::new();
    for _ in 0..20 {
        let b = b.clone();
        handles.push(tokio::spawn(
            async move { b.stat(&NodeId("x".into())).await },
        ));
    }
    for h in handles {
        h.await.unwrap().expect("stat");
    }
    // The `.expect(1)` on the token mock is verified when the server drops.
}

#[tokio::test]
async fn a_server_error_is_retryable_and_never_enoent() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/files/boom"))
        .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": {"code": 503, "message": "Backend Error"}
        })))
        .mount(&server)
        .await;

    let b = backend_with(
        &server,
        GDriveConfig {
            retry: davfs_sync::remote::RetryPolicy::none(),
            ..Default::default()
        },
    );

    let err = b.stat(&NodeId("boom".into())).await.unwrap_err();
    assert!(err.is_retryable());
    assert_ne!(
        err.errno(),
        libc::ENOENT,
        "a server error must not tell applications the file is gone"
    );
}

/// A server that is not there at all is Offline, never NotFound.
#[tokio::test]
async fn an_unreachable_api_is_offline() {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let oauth = OAuthClient::new("id", "secret")
        // Port 1 on loopback: nothing listens there.
        .with_token_endpoint("http://127.0.0.1:1/token");
    let tokens = Arc::new(TokenSource::new(http.clone(), oauth, "rt"));
    let b = GDriveBackend::new(
        http,
        tokens,
        GDriveConfig {
            api_base: "http://127.0.0.1:1".into(),
            retry: davfs_sync::remote::RetryPolicy::none(),
            ..Default::default()
        },
    )
    .with_pacer(Pacer::unlimited());

    let err = b.probe().await.unwrap_err();
    assert_eq!(err.errno(), libc::ENETUNREACH, "got {err:?}");
}

/// Every request must send the narrow `fields=` projection. Without it Drive
/// returns a default subset that omits `size`, `md5Checksum` and `parents`, and
/// every file would silently map to `SizeHint::Unknown` with no parent.
#[tokio::test]
async fn requests_send_the_field_projection() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/files/fieldcheck"))
        .and(query_param("fields", FILE_FIELDS))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "fieldcheck", "name": "a", "mimeType": "text/plain",
            "parents": ["root"], "size": "1", "version": "1"
        })))
        .mount(&server)
        .await;

    let b = backend(&server);
    // The mock only matches when fields= is exactly the projection.
    let entry = b.stat(&NodeId("fieldcheck".into())).await.expect("stat");
    assert_eq!(entry.size, SizeHint::Exact(1));
    assert_eq!(entry.parent, Some(NodeId("root".into())));
}

#[tokio::test]
async fn requests_carry_the_bearer_token() {
    let server = MockServer::start().await;
    with_token(&server).await;

    Mock::given(method("GET"))
        .and(path("/drive/v3/files/authcheck"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer test-access-token",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "authcheck", "name": "a", "mimeType": "text/plain", "version": "1"
        })))
        .mount(&server)
        .await;

    let b = backend(&server);
    // The mock matches only when the header is present and correct.
    assert!(b.stat(&NodeId("authcheck".into())).await.is_ok());
}
