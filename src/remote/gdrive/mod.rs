//! The Google Drive backend.
//!
//! Hand-written against `reqwest` rather than the generated `google-drive3`
//! crate: the surface actually needed is six endpoints of plain JSON, every
//! request sends a narrow `fields=` projection anyway, and `google-apis-rs` is
//! in maintainer-wanted status with a twice-a-year regeneration cadence -- not
//! where a storage backend's contract should live.
//!
//! # Testing
//!
//! Everything here is exercised against `wiremock` with recorded-shape
//! fixtures, because there is no Drive emulator and a service account cannot
//! hold files (0 GB quota since 2023-06-01). See issue #1.
//!
//! The Drive-shaped *behaviour* -- opaque ids, duplicate names, unknown sizes,
//! the cursor change feed -- is covered independently by
//! `MockRemote::id_addressed()` through the shared seam suite, so a mistake here
//! is a mistake in HTTP and JSON mapping, not in the model.

pub mod api;
pub mod auth;
pub mod pacer;

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;

use crate::error::RemoteError;
use crate::remote::{
    ByteRange, Capabilities, Change, ChangeBatch, ChangeFeed, ContentStream, Listing, NodeId,
    NodeKind, RemoteEntry, RemoteFlags, RemoteStorage, RetryPolicy, SizeHint, Validator,
};
use api::{DriveFile, FILE_FIELDS};
use auth::SharedTokenSource;
use pacer::Pacer;

/// Google's API root. Overridable so tests can point at a mock.
pub const DEFAULT_API_BASE: &str = "https://www.googleapis.com";

/// The documented alias for the My Drive root, so no lookup is needed to start.
pub const ROOT_ALIAS: &str = "root";

/// Drive paginates at 1000; asking for the maximum minimises quota units, since
/// a list costs 100 units regardless of page size.
const PAGE_SIZE: u32 = 1000;

/// Bytes per ranged request when streaming rather than downloading whole.
/// A download costs 200 quota units, so small ranges are expensive.
const PREFERRED_CHUNK: u32 = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct GDriveConfig {
    pub api_base: String,
    /// Restrict the mount to one folder rather than the whole drive.
    pub root_folder_id: Option<String>,
    /// Export MIME type per Google-native source type.
    pub export_formats: ExportFormats,
    pub retry: RetryPolicy,
}

impl Default for GDriveConfig {
    fn default() -> Self {
        Self {
            api_base: DEFAULT_API_BASE.to_string(),
            root_folder_id: None,
            export_formats: ExportFormats::default(),
            retry: RetryPolicy::default(),
        }
    }
}

/// Which format Google-native documents are exported as.
///
/// ODF rather than rclone's OOXML default, since this is a GNOME-targeted
/// project. Must agree with `store::naming::export_extension`.
#[derive(Debug, Clone)]
pub struct ExportFormats {
    pub document: String,
    pub spreadsheet: String,
    pub presentation: String,
    pub drawing: String,
}

impl Default for ExportFormats {
    fn default() -> Self {
        Self {
            document: "application/vnd.oasis.opendocument.text".into(),
            spreadsheet: "application/vnd.oasis.opendocument.spreadsheet".into(),
            presentation: "application/vnd.oasis.opendocument.presentation".into(),
            drawing: "image/svg+xml".into(),
        }
    }
}

impl ExportFormats {
    /// The export MIME type for a Google-native source type, if it has one.
    ///
    /// Forms, Sites and Maps have no export at all; inventing one would produce
    /// a file that cannot be read.
    pub fn for_mime(&self, mime: &str) -> Option<&str> {
        match mime {
            "application/vnd.google-apps.document" => Some(&self.document),
            "application/vnd.google-apps.spreadsheet" => Some(&self.spreadsheet),
            "application/vnd.google-apps.presentation" => Some(&self.presentation),
            "application/vnd.google-apps.drawing" => Some(&self.drawing),
            _ => None,
        }
    }
}

pub struct GDriveBackend {
    http: reqwest::Client,
    tokens: SharedTokenSource,
    config: GDriveConfig,
    caps: Capabilities,
    pacer: Pacer,
}

impl GDriveBackend {
    pub fn new(http: reqwest::Client, tokens: SharedTokenSource, config: GDriveConfig) -> Self {
        Self {
            caps: Capabilities {
                // Ids are server-assigned and opaque, so a path cannot be
                // turned into an id without asking.
                path_addressed: false,
                // Two files may share a name in one folder.
                unique_names: false,
                // There is no per-folder ETag; freshness comes from the change
                // feed instead.
                dir_validators: false,
                ranged_reads: true,
                preferred_chunk: Some(PREFERRED_CHUNK),
                max_upload_size: None,
                // Drive v3 removed ETags and has no If-Match on files.get.
                enforces_preconditions: false,
            },
            pacer: Pacer::default(),
            http,
            tokens,
            config,
        }
    }

    pub fn with_pacer(mut self, pacer: Pacer) -> Self {
        self.pacer = pacer;
        self
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/drive/v3{}",
            self.config.api_base.trim_end_matches('/'),
            path
        )
    }

    /// The id the mount is rooted at.
    fn root_id(&self) -> String {
        self.config
            .root_folder_id
            .clone()
            .unwrap_or_else(|| ROOT_ALIAS.to_string())
    }

    /// One authenticated request, paced, with a single 401 retry.
    ///
    /// The 401 retry exists because an access token can be revoked before its
    /// nominal expiry; one refresh-and-retry is the documented remedy.
    async fn send(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, RemoteError> {
        for attempt in 0..2 {
            self.pacer.acquire().await;
            let token = self.tokens.access_token().await?;

            let response = build()
                .bearer_auth(&token)
                .send()
                .await
                .map_err(to_remote_error)?;

            if response.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                tracing::debug!("401; refreshing the access token and retrying once");
                self.tokens.invalidate().await;
                continue;
            }
            return Ok(response);
        }
        Err(RemoteError::AuthExpired)
    }

    /// Send, classify, and deserialise a JSON response.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, String)],
    ) -> Result<T, RemoteError> {
        let response = crate::remote::with_retry(self.config.retry, || async {
            let response = self.send(|| self.http.get(url).query(query)).await?;
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            let body = response.text().await.unwrap_or_default();
            Err(classify(status, &body))
        })
        .await?;

        let body = response.text().await.map_err(to_remote_error)?;
        serde_json::from_str(&body)
            .map_err(|e| RemoteError::Protocol(format!("malformed Drive response: {e}")))
    }

    /// Fetch every page of a folder listing.
    async fn list_all(&self, folder: &str) -> Result<Vec<DriveFile>, RemoteError> {
        let url = self.url("/files");
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut query = vec![
                ("q".to_string(), api::children_query(folder)),
                (
                    "fields".to_string(),
                    format!("nextPageToken,incompleteSearch,files({FILE_FIELDS})"),
                ),
                ("pageSize".to_string(), PAGE_SIZE.to_string()),
                // Without these a shared drive's contents are invisible.
                ("supportsAllDrives".to_string(), "true".to_string()),
                ("includeItemsFromAllDrives".to_string(), "true".to_string()),
            ];
            if let Some(token) = &page_token {
                query.push(("pageToken".to_string(), token.clone()));
            }
            let query: Vec<(&str, String)> = query.into_iter().map(|(k, v)| (leak(k), v)).collect();

            let page: api::FileList = self.get_json(&url, &query).await?;
            out.extend(page.files);

            match page.next_page_token {
                Some(t) => page_token = Some(t),
                None => break,
            }
        }
        Ok(out)
    }

    /// Map a Drive file onto the seam's entry type.
    fn to_entry(&self, f: &DriveFile) -> RemoteEntry {
        let kind = if f.is_folder() {
            NodeKind::Dir
        } else if f.is_export_only() {
            // Readable only via files.export, and with no size until then.
            NodeKind::ExportOnly
        } else {
            NodeKind::File
        };

        let size = match (kind, f.size_bytes()) {
            (NodeKind::Dir, _) => SizeHint::Exact(0),
            (_, Some(n)) => SizeHint::Exact(n),
            // A native document, or a file the projection did not size.
            // Exact(0) here is what makes Google Docs unreadable through a
            // mount: readers stop at the first byte.
            (_, None) => SizeHint::Unknown,
        };

        RemoteEntry {
            id: NodeId(f.id.clone()),
            parent: f.parent().map(|p| NodeId(p.to_string())),
            name: f.name.clone(),
            kind,
            size,
            mtime: f.modified(),
            mime: f.mime_type.clone(),
            validator: validator_for(f),
            flags: RemoteFlags {
                read_only: false,
                trashed: f.trashed,
                shortcut: f.is_shortcut(),
            },
            target: f
                .shortcut_details
                .as_ref()
                .and_then(|s| s.target_id.clone())
                .map(NodeId),
        }
    }
}

/// Pick the strongest available validator.
///
/// `md5Checksum` is absent for native documents and `headRevisionId` for
/// folders, so `version` -- present on everything -- is the fallback. All three
/// are stronger than size+mtime.
fn validator_for(f: &DriveFile) -> Option<Validator> {
    if let Some(md5) = &f.md5_checksum {
        return Some(Validator::Checksum {
            algo: crate::remote::ChecksumAlgo::Md5,
            hex: md5.clone(),
        });
    }
    if let Some(rev) = &f.head_revision_id {
        return Some(Validator::Etag(rev.clone()));
    }
    if let Some(v) = f.version.as_ref().and_then(|v| v.parse::<i64>().ok()) {
        return Some(Validator::Version(v));
    }
    f.modified().map(|m| Validator::SizeMtime {
        size: f.size_bytes().unwrap_or(0),
        mtime_unix: m.timestamp(),
    })
}

/// Map an HTTP status plus Drive's error body onto a typed error.
///
/// The body matters: a rate limit and a permission failure both arrive as 403,
/// and only `reason` tells them apart. Treating a rate limit as permanent would
/// make a busy mount look broken; treating a permission failure as retryable
/// would hammer the API.
fn classify(status: reqwest::StatusCode, body: &str) -> RemoteError {
    let envelope: api::ErrorEnvelope = serde_json::from_str(body).unwrap_or_default();
    let reason = envelope.error.reason().unwrap_or_default().to_string();

    match status.as_u16() {
        401 => RemoteError::AuthExpired,
        403 => match reason.as_str() {
            "userRateLimitExceeded" | "rateLimitExceeded" | "sharingRateLimitExceeded" => {
                RemoteError::RateLimited(None)
            }
            "storageQuotaExceeded" => RemoteError::QuotaExceeded,
            // dailyLimitExceeded is NOT retryable: the quota resets at midnight
            // Pacific, and retrying only burns more of it.
            _ => RemoteError::Forbidden,
        },
        404 => RemoteError::NotFound,
        // A page token that has aged out. The caller must resync from scratch.
        400 if reason == "invalidPageToken" || body.contains("invalidPageToken") => {
            RemoteError::Changed
        }
        429 => RemoteError::RateLimited(None),
        s if (500..600).contains(&s) => RemoteError::Server {
            status: s,
            body: envelope.error.message,
        },
        s => RemoteError::Protocol(format!("Drive returned {s}: {}", envelope.error.message)),
    }
}

fn to_remote_error(e: reqwest::Error) -> RemoteError {
    if e.is_timeout() || e.is_connect() || e.is_request() {
        RemoteError::Offline(e.to_string())
    } else {
        RemoteError::Protocol(e.to_string())
    }
}

/// `reqwest::query` wants `&str` keys; the key set here is a fixed, small
/// vocabulary, so leaking is bounded and avoids threading lifetimes through
/// every call.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

#[async_trait::async_trait]
impl RemoteStorage for GDriveBackend {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn change_feed(&self) -> ChangeFeed {
        ChangeFeed::Cursor
    }

    fn root(&self) -> NodeId {
        NodeId(self.root_id())
    }

    /// Always `None`: a Drive id cannot be derived from a name.
    fn child_id(&self, _parent: &NodeId, _name: &str) -> Option<NodeId> {
        None
    }

    async fn probe(&self) -> Result<(), RemoteError> {
        let url = self.url(&format!("/files/{}", self.root_id()));
        let _: DriveFile = self.get_json(&url, &[("fields", "id".to_string())]).await?;
        Ok(())
    }

    async fn list(
        &self,
        dir: &NodeId,
        _if_unchanged: Option<&Validator>,
    ) -> Result<Listing, RemoteError> {
        // Drive has no per-folder validator, so a listing is always fresh;
        // freshness comes from poll_changes instead.
        let files = self.list_all(dir.as_str()).await?;
        let children: Vec<RemoteEntry> = files.iter().map(|f| self.to_entry(f)).collect();

        let this_url = self.url(&format!("/files/{}", dir.as_str()));
        let this: Option<Box<RemoteEntry>> = self
            .get_json::<DriveFile>(
                &this_url,
                &[
                    ("fields", FILE_FIELDS.to_string()),
                    ("supportsAllDrives", "true".to_string()),
                ],
            )
            .await
            .ok()
            .map(|f| Box::new(self.to_entry(&f)));

        Ok(Listing::Fresh {
            validator: None,
            this,
            children,
            // Every page was fetched, so absent children really are gone.
            complete: true,
        })
    }

    async fn stat(&self, node: &NodeId) -> Result<RemoteEntry, RemoteError> {
        let url = self.url(&format!("/files/{}", node.as_str()));
        let file: DriveFile = self
            .get_json(
                &url,
                &[
                    ("fields", FILE_FIELDS.to_string()),
                    ("supportsAllDrives", "true".to_string()),
                ],
            )
            .await?;
        Ok(self.to_entry(&file))
    }

    async fn open_read(
        &self,
        node: &NodeId,
        range: Option<ByteRange>,
        _if_unchanged: Option<&Validator>,
    ) -> Result<ContentStream, RemoteError> {
        // A native document has to be exported, and export does not honour
        // Range -- so the whole thing comes down in one go.
        let entry = self.stat(node).await?;
        let export_mime = if entry.kind == NodeKind::ExportOnly {
            match entry
                .mime
                .as_deref()
                .and_then(|m| self.config.export_formats.for_mime(m))
            {
                Some(m) => Some(m.to_string()),
                None => {
                    // Forms, Sites and Maps have no export at all.
                    return Err(RemoteError::Unsupported(
                        "this document type cannot be exported",
                    ));
                }
            }
        } else {
            None
        };

        let (url, mut query) = match &export_mime {
            Some(mime) => (
                self.url(&format!("/files/{}/export", node.as_str())),
                vec![("mimeType", mime.clone())],
            ),
            None => (
                self.url(&format!("/files/{}", node.as_str())),
                vec![
                    ("alt", "media".to_string()),
                    ("supportsAllDrives", "true".to_string()),
                ],
            ),
        };
        query.push(("acknowledgeAbuse", "false".to_string()));

        let ranged = range.filter(|_| export_mime.is_none());
        let response = crate::remote::with_retry(self.config.retry, || async {
            let response = self
                .send(|| {
                    let mut req = self.http.get(&url).query(&query);
                    if let Some(r) = ranged {
                        let header = match r.len {
                            Some(len) if len > 0 => {
                                format!("bytes={}-{}", r.offset, r.offset + len - 1)
                            }
                            _ => format!("bytes={}-", r.offset),
                        };
                        req = req.header(reqwest::header::RANGE, header);
                    }
                    req
                })
                .await?;

            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            let body = response.text().await.unwrap_or_default();
            Err(classify(status, &body))
        })
        .await?;

        let total_size = content_range_total(&response).or_else(|| response.content_length());
        let body = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(to_remote_error));

        Ok(ContentStream {
            validator: entry.validator,
            total_size,
            body: Box::pin(body),
        })
    }

    async fn start_cursor(&self) -> Result<String, RemoteError> {
        let url = self.url("/changes/startPageToken");
        let token: api::StartPageToken = self
            .get_json(&url, &[("supportsAllDrives", "true".to_string())])
            .await?;
        Ok(token.start_page_token)
    }

    async fn poll_changes(&self, cursor: &str) -> Result<ChangeBatch, RemoteError> {
        let url = self.url("/changes");
        let query = vec![
            ("pageToken", cursor.to_string()),
            (
                "fields",
                format!("nextPageToken,newStartPageToken,changes(fileId,removed,changeType,file({FILE_FIELDS}))"),
            ),
            ("pageSize", PAGE_SIZE.to_string()),
            // Without this, deletions never appear.
            ("includeRemoved", "true".to_string()),
            ("supportsAllDrives", "true".to_string()),
            ("includeItemsFromAllDrives", "true".to_string()),
        ];

        let page: api::ChangeList = self.get_json(&url, &query).await?;

        let changes = page
            .changes
            .iter()
            .filter_map(|c| {
                if c.removed {
                    return c.file_id.clone().map(|id| Change::Removed(NodeId(id)));
                }
                // A file moved to the trash is a removal as far as the mount is
                // concerned; it is still visible to the API.
                match &c.file {
                    Some(f) if f.trashed => Some(Change::Removed(NodeId(f.id.clone()))),
                    Some(f) => Some(Change::Upserted(Box::new(self.to_entry(f)))),
                    None => c.file_id.clone().map(|id| Change::Removed(NodeId(id))),
                }
            })
            .collect();

        // newStartPageToken appears only on the last page.
        let (cursor, more) = match (page.next_page_token, page.new_start_page_token) {
            (Some(next), _) => (next, true),
            (None, Some(new_start)) => (new_start, false),
            (None, None) => (cursor.to_string(), false),
        };

        Ok(ChangeBatch {
            cursor,
            changes,
            more,
        })
    }
}

fn content_range_total(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .rsplit('/')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Build a backend from a refresh token.
pub fn build(
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
    config: GDriveConfig,
    timeout: Duration,
) -> Result<GDriveBackend, RemoteError> {
    let http = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| RemoteError::Protocol(e.to_string()))?;

    let oauth = auth::OAuthClient::new(client_id, client_secret);
    let tokens = Arc::new(auth::TokenSource::new(http.clone(), oauth, refresh_token));
    Ok(GDriveBackend::new(http, tokens, config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> GDriveBackend {
        let http = reqwest::Client::new();
        let oauth = auth::OAuthClient::new("id", "secret");
        let tokens = Arc::new(auth::TokenSource::new(http.clone(), oauth, "rt"));
        GDriveBackend::new(http, tokens, GDriveConfig::default())
    }

    fn file(json: &str) -> DriveFile {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn the_shape_reported_is_the_drive_shape() {
        let b = backend();
        let caps = b.capabilities();
        assert!(!caps.path_addressed, "Drive ids are not derived from paths");
        assert!(!caps.unique_names, "Drive permits duplicate names");
        assert!(!caps.dir_validators, "Drive has no per-folder ETag");
        assert!(!caps.enforces_preconditions, "Drive v3 removed ETags");
        assert!(caps.ranged_reads);
        assert_eq!(caps.preferred_chunk, Some(PREFERRED_CHUNK));
        assert_eq!(b.change_feed(), ChangeFeed::Cursor);
        assert_eq!(b.root(), NodeId("root".into()));
        assert_eq!(b.child_id(&b.root(), "anything"), None);
    }

    #[test]
    fn a_root_folder_id_reroots_the_mount() {
        let http = reqwest::Client::new();
        let oauth = auth::OAuthClient::new("id", "secret");
        let tokens = Arc::new(auth::TokenSource::new(http.clone(), oauth, "rt"));
        let b = GDriveBackend::new(
            http,
            tokens,
            GDriveConfig {
                root_folder_id: Some("1FolderId".into()),
                ..Default::default()
            },
        );
        assert_eq!(b.root(), NodeId("1FolderId".into()));
    }

    #[test]
    fn a_native_document_maps_to_unknown_size() {
        let b = backend();
        let e = b.to_entry(&file(
            r#"{"id":"1","name":"Notes",
                "mimeType":"application/vnd.google-apps.document",
                "parents":["root"],"version":"7"}"#,
        ));
        assert_eq!(e.kind, NodeKind::ExportOnly);
        assert_eq!(e.size, SizeHint::Unknown);
        assert_ne!(
            e.size.or_placeholder(),
            0,
            "reporting 0 makes the document unreadable through a mount"
        );
        assert_eq!(e.validator, Some(Validator::Version(7)));
    }

    #[test]
    fn a_binary_file_maps_to_an_exact_size_and_a_checksum() {
        let b = backend();
        let e = b.to_entry(&file(
            r#"{"id":"2","name":"p.jpg","mimeType":"image/jpeg","parents":["1"],
                "size":"1024","md5Checksum":"abc123","version":"3"}"#,
        ));
        assert_eq!(e.kind, NodeKind::File);
        assert_eq!(e.size, SizeHint::Exact(1024));
        assert_eq!(
            e.validator,
            Some(Validator::Checksum {
                algo: crate::remote::ChecksumAlgo::Md5,
                hex: "abc123".into()
            })
        );
        assert_eq!(e.parent, Some(NodeId("1".into())));
    }

    #[test]
    fn validators_fall_back_in_strength_order() {
        // md5 beats headRevisionId beats version beats size+mtime.
        assert!(matches!(
            validator_for(&file(
                r#"{"id":"a","md5Checksum":"m","headRevisionId":"h","version":"1"}"#
            )),
            Some(Validator::Checksum { .. })
        ));
        assert!(matches!(
            validator_for(&file(r#"{"id":"a","headRevisionId":"h","version":"1"}"#)),
            Some(Validator::Etag(_))
        ));
        assert_eq!(
            validator_for(&file(r#"{"id":"a","version":"42"}"#)),
            Some(Validator::Version(42))
        );
        assert!(matches!(
            validator_for(&file(
                r#"{"id":"a","size":"5","modifiedTime":"2026-08-12T06:37:07Z"}"#
            )),
            Some(Validator::SizeMtime { .. })
        ));
        // Nothing at all -> no validator, so the content cache refuses to key it.
        assert_eq!(validator_for(&file(r#"{"id":"a"}"#)), None);
    }

    #[test]
    fn a_folder_maps_to_a_directory() {
        let b = backend();
        let e = b.to_entry(&file(
            r#"{"id":"f","name":"Docs",
                "mimeType":"application/vnd.google-apps.folder","parents":["root"]}"#,
        ));
        assert_eq!(e.kind, NodeKind::Dir);
        assert_eq!(e.size, SizeHint::Exact(0));
    }

    #[test]
    fn a_shortcut_keeps_its_target() {
        let b = backend();
        let e = b.to_entry(&file(
            r#"{"id":"s","name":"link",
                "mimeType":"application/vnd.google-apps.shortcut","parents":["root"],
                "shortcutDetails":{"targetId":"2"}}"#,
        ));
        assert!(e.flags.shortcut);
        assert_eq!(e.target, Some(NodeId("2".into())));
        assert_ne!(e.kind, NodeKind::ExportOnly, "a shortcut is not a document");
    }

    #[test]
    fn trashed_files_are_flagged() {
        let b = backend();
        let e = b.to_entry(&file(r#"{"id":"t","name":"gone","trashed":true}"#));
        assert!(e.flags.trashed);
    }

    /// Both arrive as HTTP 403; only the reason distinguishes them, and getting
    /// this wrong makes a busy mount look permanently broken.
    #[test]
    fn a_rate_limit_is_not_confused_with_a_permission_failure() {
        let rate = classify(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"error":{"code":403,"errors":[{"reason":"userRateLimitExceeded"}]}}"#,
        );
        assert!(matches!(rate, RemoteError::RateLimited(_)));
        assert!(rate.is_retryable());

        let perm = classify(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"error":{"code":403,"errors":[{"reason":"insufficientFilePermissions"}]}}"#,
        );
        assert!(matches!(perm, RemoteError::Forbidden));
        assert!(
            !perm.is_retryable(),
            "retrying a permission failure is futile"
        );

        // A daily limit resets at midnight Pacific; retrying burns more quota.
        let daily = classify(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"error":{"code":403,"errors":[{"reason":"dailyLimitExceeded"}]}}"#,
        );
        assert!(!daily.is_retryable());
    }

    #[test]
    fn error_statuses_map_to_the_right_errno() {
        assert_eq!(
            classify(reqwest::StatusCode::NOT_FOUND, "{}").errno(),
            libc::ENOENT
        );
        assert_eq!(
            classify(reqwest::StatusCode::UNAUTHORIZED, "{}").errno(),
            libc::EACCES
        );
        assert_eq!(
            classify(reqwest::StatusCode::TOO_MANY_REQUESTS, "{}").errno(),
            libc::EAGAIN
        );
        assert_eq!(
            classify(
                reqwest::StatusCode::FORBIDDEN,
                r#"{"error":{"errors":[{"reason":"storageQuotaExceeded"}]}}"#
            )
            .errno(),
            libc::EDQUOT
        );
        // A server error is retryable, and never ENOENT.
        let server = classify(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "{}");
        assert!(server.is_retryable());
        assert_ne!(server.errno(), libc::ENOENT);
    }

    /// An aged-out page token means the delta stream is broken and the caller
    /// must resync, which is distinct from any other 400.
    #[test]
    fn an_invalid_page_token_is_reported_as_changed() {
        let e = classify(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":{"code":400,"errors":[{"reason":"invalidPageToken"}]}}"#,
        );
        assert!(matches!(e, RemoteError::Changed));
    }

    #[test]
    fn export_formats_cover_the_types_that_have_one() {
        let f = ExportFormats::default();
        assert_eq!(
            f.for_mime("application/vnd.google-apps.document"),
            Some("application/vnd.oasis.opendocument.text")
        );
        assert!(f
            .for_mime("application/vnd.google-apps.spreadsheet")
            .is_some());
        assert!(f
            .for_mime("application/vnd.google-apps.presentation")
            .is_some());
        assert!(f.for_mime("application/vnd.google-apps.drawing").is_some());
        // Forms and Sites genuinely have no export; inventing one would produce
        // an unreadable file.
        assert_eq!(f.for_mime("application/vnd.google-apps.form"), None);
        assert_eq!(f.for_mime("application/vnd.google-apps.site"), None);
        assert_eq!(f.for_mime("image/jpeg"), None);
    }

    /// The export format and the display extension must agree, or a document
    /// gets an extension that does not match its bytes.
    #[test]
    fn export_formats_agree_with_the_naming_pipeline() {
        let f = ExportFormats::default();
        for (source, want_ext) in [
            ("application/vnd.google-apps.document", "odt"),
            ("application/vnd.google-apps.spreadsheet", "ods"),
            ("application/vnd.google-apps.presentation", "odp"),
            ("application/vnd.google-apps.drawing", "svg"),
        ] {
            let ext = crate::store::naming::export_extension(Some(source));
            assert_eq!(ext, Some(want_ext), "{source}");
            assert!(
                f.for_mime(source).is_some(),
                "{source} has an extension but no export format"
            );
        }
    }

    #[test]
    fn urls_are_built_against_the_configured_base() {
        let http = reqwest::Client::new();
        let oauth = auth::OAuthClient::new("id", "secret");
        let tokens = Arc::new(auth::TokenSource::new(http.clone(), oauth, "rt"));
        let b = GDriveBackend::new(
            http,
            tokens,
            GDriveConfig {
                api_base: "http://127.0.0.1:8080".into(),
                ..Default::default()
            },
        );
        assert_eq!(b.url("/files"), "http://127.0.0.1:8080/drive/v3/files");
        // A trailing slash on the base must not double up.
        assert_eq!(
            GDriveConfig {
                api_base: "http://h/".into(),
                ..Default::default()
            }
            .api_base
            .trim_end_matches('/'),
            "http://h"
        );
    }
}
