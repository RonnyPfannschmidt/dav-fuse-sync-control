use bytes::Bytes;
use reqwest::{Client, StatusCode};
use std::time::Duration;
use url::Url;

use super::propfind::{parse_multistatus, propfind_body, resolve, ResolvedListing};
use super::{
    ByteRange, Capabilities, ChangeFeed, ContentSource, ContentStream, Listing, NodeId, NodeKind,
    RemoteEntry, RemoteFlags, RemoteStorage, RetryPolicy, SizeHint, Validator,
};
use crate::error::RemoteError;

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DavEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
}

/// The outcome of a directory listing.
///
/// Replaces the PoC's empty-`Vec` sentinel for 304, which was indistinguishable
/// from "this directory is empty". Two bugs cancelled out there: the caller
/// guarded with `if !new_entries.is_empty()`, so a directory whose last file was
/// deleted could never be written back to the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListResult {
    /// The server confirmed nothing changed; the cached listing stands.
    NotModified,
    Modified {
        entries: Vec<DavEntry>,
        /// Validator for the *directory*, taken from the self-entry's
        /// `<getetag>` in the body -- a 207 has no ETag response header.
        etag: Option<String>,
    },
}

#[derive(Clone)]
pub struct WebDavClient {
    client: Client,
    /// Always ends in `/`, which `Url::make_relative` requires and which keeps
    /// `path_segments_mut` from eating the last segment.
    base_url: Url,
    username: String,
    password: String,
}

impl WebDavClient {
    pub fn new(base_url: String, username: String, password: String) -> Result<Self, RemoteError> {
        Self::with_timeout(base_url, username, password, Duration::from_secs(30))
    }

    pub fn with_timeout(
        base_url: String,
        username: String,
        password: String,
        timeout: Duration,
    ) -> Result<Self, RemoteError> {
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| RemoteError::Protocol(format!("building HTTP client: {e}")))?;

        let mut base_url = Url::parse(&base_url)
            .map_err(|e| RemoteError::Protocol(format!("invalid base URL: {e}")))?;

        // Normalise to a trailing slash. Without it `Url::join("sub")` silently
        // drops the last path segment -- so a base of
        // `https://h/remote.php/dav/files/alice` resolved `sub` to
        // `https://h/remote.php/dav/files/sub`. `make_relative` also requires it.
        if !base_url.path().ends_with('/') {
            let path = format!("{}/", base_url.path());
            base_url.set_path(&path);
        }

        Ok(Self {
            client,
            base_url,
            username,
            password,
        })
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Build the URL for a path below the base.
    ///
    /// Never uses `Url::join`, which parses its argument as a relative *URL
    /// reference*: a name containing `#`, `?` or `%` is truncated or
    /// misinterpreted, and a base without a trailing slash loses a segment.
    /// Pushing segments percent-encodes each one correctly.
    fn url_for(&self, path: &str, directory: bool) -> Url {
        let mut url = self.base_url.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .expect("base URL is hierarchical; checked in new()");
            // Drop the empty segment the trailing slash produced.
            segments.pop_if_empty();
            for part in path.split('/').filter(|s| !s.is_empty()) {
                segments.push(part);
            }
            // Collections want a trailing slash or servers answer 301.
            if directory {
                segments.push("");
            }
        }
        url
    }

    /// A cheap liveness and credentials probe.
    pub async fn test_connection(&self) -> Result<(), RemoteError> {
        let response = self
            .request("PROPFIND", self.base_url.clone())
            .header("Depth", "0")
            .header("Content-Type", "application/xml")
            .body(propfind_body())
            .send()
            .await
            .map_err(to_remote_error)?;

        check_status(response.status(), &self.base_url)?;
        Ok(())
    }

    /// List one directory.
    ///
    /// `if_none_match` is the validator held for this directory's listing. On a
    /// 304 the result is [`ListResult::NotModified`].
    pub async fn list_dir(
        &self,
        path: &str,
        if_none_match: Option<&str>,
    ) -> Result<ListResult, RemoteError> {
        let url = self.url_for(path, true);

        let mut request = self
            .request("PROPFIND", url.clone())
            // Depth: 1 only. `Depth: infinity` is rejected outright by
            // Nextcloud and SabreDAV with 403 Propfind-Finite-Depth.
            .header("Depth", "1")
            .header("Content-Type", "application/xml");

        if let Some(etag) = if_none_match {
            request = request.header("If-None-Match", etag);
        }

        let response = request
            .body(propfind_body())
            .send()
            .await
            .map_err(to_remote_error)?;

        if response.status() == StatusCode::NOT_MODIFIED {
            tracing::debug!("304 for {}", url);
            return Ok(ListResult::NotModified);
        }

        check_status(response.status(), &url)?;

        let body = response.text().await.map_err(to_remote_error)?;
        let responses = parse_multistatus(&body)?;
        // Resolve against the directory being listed, not the account root.
        // Relative to the root, the self-entry of /nested/a/b/c/ has the
        // non-empty path "nested/a/b/c" and is indistinguishable from a child.
        let listing: ResolvedListing = resolve(&url, &responses);

        // The directory's validator comes from its own entry in the body.
        let etag = listing.this.as_ref().and_then(|t| t.etag.clone());

        // Belt and braces for servers that ignore If-None-Match and answer 207
        // anyway: compare the validators ourselves and report NotModified.
        if let (Some(want), Some(got)) = (if_none_match, etag.as_deref()) {
            if want == got {
                tracing::debug!("server ignored If-None-Match; validators match for {}", url);
                return Ok(ListResult::NotModified);
            }
        }

        let entries = listing
            .children
            .into_iter()
            .map(|c| DavEntry {
                name: c.name,
                is_dir: c.is_dir,
                size: c.size,
                modified: c.mtime,
            })
            .collect();

        Ok(ListResult::Modified { entries, etag })
    }

    /// Like [`Self::list_dir`], but keeping every property the parser found.
    ///
    /// `list_dir` flattens into `DavEntry`, which has no etag field -- so the
    /// per-file entity tags go on the floor. The content cache keys blobs by
    /// (identity, validator), so it wants them.
    ///
    /// `Ok(None)` means not modified.
    pub async fn list_dir_resolved(
        &self,
        path: &str,
        if_none_match: Option<&str>,
    ) -> Result<Option<ResolvedListing>, RemoteError> {
        let url = self.url_for(path, true);

        let mut request = self
            .request("PROPFIND", url.clone())
            .header("Depth", "1")
            .header("Content-Type", "application/xml");
        if let Some(etag) = if_none_match {
            request = request.header("If-None-Match", etag);
        }

        let response = request
            .body(propfind_body())
            .send()
            .await
            .map_err(to_remote_error)?;

        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        check_status(response.status(), &url)?;

        let body = response.text().await.map_err(to_remote_error)?;
        let responses = parse_multistatus(&body)?;
        let listing = resolve(&url, &responses);

        // Servers that ignore If-None-Match answer 207 anyway; compare the
        // directory validator ourselves and still report "unchanged".
        if let (Some(want), Some(got)) = (
            if_none_match,
            listing.this.as_ref().and_then(|t| t.etag.as_deref()),
        ) {
            if want == got {
                return Ok(None);
            }
        }

        Ok(Some(listing))
    }

    /// Download a whole file.
    pub async fn read_file(&self, path: &str) -> Result<Vec<u8>, RemoteError> {
        let url = self.url_for(path, false);
        let response = self
            .request("GET", url.clone())
            .send()
            .await
            .map_err(to_remote_error)?;

        check_status(response.status(), &url)?;
        Ok(response.bytes().await.map_err(to_remote_error)?.to_vec())
    }

    /// Public form of a path's URL, for headers like `Destination`.
    pub fn url_for_public(&self, path: &str) -> String {
        self.url_for(path, false).to_string()
    }

    /// Stream a file's content, optionally a byte range.
    ///
    /// Streaming rather than `Vec<u8>`: a whole-file `read_file` cannot express
    /// resume, and buffering a multi-gigabyte file in memory to satisfy a
    /// 128 KiB FUSE read is not an option.
    pub async fn get_stream(
        &self,
        path: &str,
        range: Option<ByteRange>,
        if_match: Option<&str>,
    ) -> Result<ContentStream, RemoteError> {
        use futures::StreamExt;

        let url = self.url_for(path, false);
        let mut req = self.request("GET", url.clone());

        if let Some(r) = range {
            let header = match r.len {
                Some(len) if len > 0 => format!("bytes={}-{}", r.offset, r.offset + len - 1),
                _ => format!("bytes={}-", r.offset),
            };
            req = req.header("Range", header);
        }
        if let Some(etag) = if_match {
            // If-Range on a ranged request: the server sends the whole file
            // (200) instead of a mismatched range, which is what makes resume
            // safe. On a full request If-Match turns a changed file into a 412
            // rather than silently returning different bytes.
            req = if range.is_some() {
                req.header("If-Range", etag)
            } else {
                req.header("If-Match", etag)
            };
        }

        let response = req.send().await.map_err(to_remote_error)?;
        check_status(response.status(), &url)?;

        let validator = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| Validator::Etag(s.to_string()));

        let total_size = content_range_total(&response).or_else(|| response.content_length());

        let body = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(to_remote_error));

        Ok(ContentStream {
            validator,
            total_size,
            body: Box::pin(body),
        })
    }

    /// Create a collection.
    pub async fn mkcol(&self, path: &str) -> Result<(), RemoteError> {
        let url = self.url_for(path, true);
        let response = self
            .request("MKCOL", url.clone())
            .send()
            .await
            .map_err(to_remote_error)?;
        check_status(response.status(), &url)
    }

    /// Upload content. Returns the new entity tag when the server provides one.
    pub async fn put(
        &self,
        path: &str,
        body: ContentSource,
        if_match: Option<&str>,
        mime: Option<&str>,
    ) -> Result<Option<String>, RemoteError> {
        let url = self.url_for(path, false);
        let mut req = self.request("PUT", url.clone());

        if let Some(etag) = if_match {
            // The whole point of the parameter: a 412 instead of a lost update.
            req = req.header("If-Match", etag);
        } else {
            // Refuse to overwrite something that appeared since we last looked.
            req = req.header("If-None-Match", "*");
        }
        if let Some(m) = mime {
            req = req.header("Content-Type", m);
        }

        let req = match body {
            ContentSource::Bytes(b) => req.body(b),
            ContentSource::File { path, .. } => {
                let file = tokio::fs::File::open(&path).await?;
                req.body(reqwest::Body::wrap_stream(
                    tokio_util::io::ReaderStream::new(file),
                ))
            }
        };

        let response = req.send().await.map_err(to_remote_error)?;
        check_status(response.status(), &url)?;

        Ok(response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()))
    }

    pub async fn delete(&self, path: &str, if_match: Option<&str>) -> Result<(), RemoteError> {
        let url = self.url_for(path, false);
        let mut req = self.request("DELETE", url.clone());
        if let Some(etag) = if_match {
            req = req.header("If-Match", etag);
        }
        let response = req.send().await.map_err(to_remote_error)?;
        check_status(response.status(), &url)
    }

    pub async fn move_to(&self, path: &str, destination: &str) -> Result<(), RemoteError> {
        let url = self.url_for(path, false);
        let response = self
            .request("MOVE", url.clone())
            .header("Destination", destination)
            // Do not clobber an existing file at the destination.
            .header("Overwrite", "F")
            .send()
            .await
            .map_err(to_remote_error)?;
        check_status(response.status(), &url)
    }

    fn request(&self, method: &str, url: Url) -> reqwest::RequestBuilder {
        let method =
            reqwest::Method::from_bytes(method.as_bytes()).expect("method names here are literals");
        self.client
            .request(method, url)
            .basic_auth(&self.username, Some(&self.password))
    }
}

/// Total length from a `Content-Range: bytes X-Y/Z` response header.
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

/// Map an HTTP status onto a typed error.
///
/// The PoC bailed with `anyhow` on any non-success status and the FUSE layer
/// turned every one of them into `ENOENT` -- so a rate-limit reply told
/// applications the file did not exist.
fn check_status(status: StatusCode, url: &Url) -> Result<(), RemoteError> {
    if status.is_success() {
        return Ok(());
    }
    Err(match status {
        StatusCode::UNAUTHORIZED => RemoteError::Unauthorized,
        StatusCode::FORBIDDEN => RemoteError::Forbidden,
        StatusCode::NOT_FOUND | StatusCode::GONE => RemoteError::NotFound,
        StatusCode::CONFLICT | StatusCode::METHOD_NOT_ALLOWED => RemoteError::Exists,
        StatusCode::PRECONDITION_FAILED => RemoteError::Changed,
        StatusCode::LOCKED => RemoteError::Locked,
        StatusCode::TOO_MANY_REQUESTS => RemoteError::RateLimited(None),
        StatusCode::INSUFFICIENT_STORAGE => RemoteError::QuotaExceeded,
        s if s.is_server_error() => RemoteError::Server {
            status: s.as_u16(),
            body: format!("{s} from {url}"),
        },
        s => RemoteError::Protocol(format!("unexpected status {s} from {url}")),
    })
}

/// Distinguish "we cannot reach the server" from "the server said no".
///
/// Only the former justifies serving stale cache.
fn to_remote_error(e: reqwest::Error) -> RemoteError {
    if e.is_timeout() || e.is_connect() || e.is_request() {
        RemoteError::Offline(e.to_string())
    } else {
        RemoteError::Protocol(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// RemoteStorage implementation
// ---------------------------------------------------------------------------

/// [`WebDavClient`] behind the backend seam.
///
/// A [`NodeId`] here is the percent-decoded path below the account root: a
/// leading `/`, no trailing one, and the root is exactly `"/"`. That makes
/// `path_addressed` true, so the VFS can derive a child id without a round
/// trip.
pub struct WebDavBackend {
    client: WebDavClient,
    caps: Capabilities,
    retry: RetryPolicy,
}

impl WebDavBackend {
    pub fn new(client: WebDavClient) -> Self {
        Self {
            client,
            caps: Capabilities {
                path_addressed: true,
                unique_names: true,
                // Depends on the server actually returning getetag for
                // collections; when it does not, `list` falls back to
                // SizeMtime and revalidation still works, just less cheaply.
                dir_validators: true,
                ranged_reads: true,
                // No preference: whole-file download is the right default over
                // a WAN, where one ranged GET per 128 KiB FUSE read is 30-100x
                // slower and invites rate limiting.
                preferred_chunk: None,
                max_upload_size: None,
                // Assumed, not verified: RFC 4918 requires it and Nextcloud
                // implements it, but rclone's server silently ignores every
                // conditional-write header.
                enforces_preconditions: true,
            },
            retry: RetryPolicy::default(),
        }
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn client(&self) -> &WebDavClient {
        &self.client
    }

    /// `NodeId` -> the path form the HTTP layer wants (no leading slash).
    fn dav_path(id: &NodeId) -> &str {
        id.as_str().trim_start_matches('/')
    }

    /// Join a child onto a parent id, normalising the root.
    fn join(parent: &NodeId, name: &str) -> NodeId {
        if parent.as_str() == "/" {
            NodeId(format!("/{name}"))
        } else {
            NodeId(format!(
                "{}/{}",
                parent.as_str().trim_end_matches('/'),
                name
            ))
        }
    }

    fn parent_of(id: &NodeId) -> Option<NodeId> {
        let s = id.as_str();
        if s == "/" {
            return None;
        }
        Some(match s.rfind('/') {
            Some(0) | None => NodeId("/".into()),
            Some(i) => NodeId(s[..i].to_string()),
        })
    }

    fn entry_from(parent: &NodeId, e: &crate::remote::propfind::ResolvedEntry) -> RemoteEntry {
        let id = Self::join(parent, &e.name);
        RemoteEntry {
            parent: Some(parent.clone()),
            name: e.name.clone(),
            kind: if e.is_dir {
                NodeKind::Dir
            } else {
                NodeKind::File
            },
            size: SizeHint::Exact(e.size),
            mtime: e.mtime,
            mime: e.content_type.clone(),
            // Prefer the entity tag; fall back to size+mtime so the content
            // cache always has a key, even on a bare-bones server.
            validator: e.etag.clone().map(Validator::Etag).or_else(|| {
                e.mtime.map(|m| Validator::SizeMtime {
                    size: e.size,
                    mtime_unix: m.timestamp(),
                })
            }),
            flags: RemoteFlags::default(),
            target: None,
            id,
        }
    }
}

#[async_trait::async_trait]
impl RemoteStorage for WebDavBackend {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn change_feed(&self) -> ChangeFeed {
        ChangeFeed::PerDirectory
    }

    fn root(&self) -> NodeId {
        NodeId("/".into())
    }

    fn child_id(&self, parent: &NodeId, name: &str) -> Option<NodeId> {
        Some(Self::join(parent, name))
    }

    async fn probe(&self) -> Result<(), RemoteError> {
        super::with_retry(self.retry, || self.client.test_connection()).await
    }

    async fn list(
        &self,
        dir: &NodeId,
        if_unchanged: Option<&Validator>,
    ) -> Result<Listing, RemoteError> {
        let path = Self::dav_path(dir);
        // Only an entity tag can go in If-None-Match; other validator shapes
        // are compared locally after the fact.
        let etag = if_unchanged.and_then(|v| v.as_http_etag());

        let listing =
            super::with_retry(self.retry, || self.client.list_dir_resolved(path, etag)).await?;

        let Some(listing) = listing else {
            return Ok(Listing::Unchanged);
        };

        let children = listing
            .children
            .iter()
            .map(|e| Self::entry_from(dir, e))
            .collect();

        let dir_validator = listing
            .this
            .as_ref()
            .and_then(|t| t.etag.clone())
            .map(Validator::Etag);

        let this = listing.this.as_ref().map(|t| {
            Box::new(RemoteEntry {
                id: dir.clone(),
                parent: Self::parent_of(dir),
                name: t.name.clone(),
                kind: NodeKind::Dir,
                size: SizeHint::Exact(0),
                mtime: t.mtime,
                mime: None,
                validator: dir_validator.clone(),
                flags: RemoteFlags::default(),
                target: None,
            })
        });

        Ok(Listing::Fresh {
            validator: dir_validator,
            this,
            children,
            // Depth: 1 always returns the whole directory in one response.
            complete: true,
        })
    }

    async fn stat(&self, node: &NodeId) -> Result<RemoteEntry, RemoteError> {
        match Self::parent_of(node) {
            // The root has no parent to scan; describe it directly.
            None => Ok(RemoteEntry {
                id: node.clone(),
                parent: None,
                name: String::new(),
                kind: NodeKind::Dir,
                size: SizeHint::Exact(0),
                mtime: None,
                mime: None,
                validator: None,
                flags: RemoteFlags::default(),
                target: None,
            }),
            Some(parent) => {
                let name = node.as_str().rsplit('/').next().unwrap_or_default();
                self.lookup(&parent, name).await
            }
        }
    }

    async fn open_read(
        &self,
        node: &NodeId,
        range: Option<ByteRange>,
        if_unchanged: Option<&Validator>,
    ) -> Result<ContentStream, RemoteError> {
        let path = Self::dav_path(node);
        self.client
            .get_stream(path, range, if_unchanged.and_then(|v| v.as_http_etag()))
            .await
    }

    // ---- write half: implemented and tested, but never called from FUSE ---

    async fn create_dir(&self, parent: &NodeId, name: &str) -> Result<RemoteEntry, RemoteError> {
        let id = Self::join(parent, name);
        self.client.mkcol(Self::dav_path(&id)).await?;
        Ok(RemoteEntry {
            parent: Some(parent.clone()),
            ..RemoteEntry::dir(id, name)
        })
    }

    async fn create_file(
        &self,
        parent: &NodeId,
        name: &str,
        mime: Option<&str>,
    ) -> Result<RemoteEntry, RemoteError> {
        let id = Self::join(parent, name);
        // WebDAV has no "create empty"; an empty PUT is the idiom.
        let etag = self
            .client
            .put(
                Self::dav_path(&id),
                ContentSource::Bytes(Bytes::new()),
                None,
                mime,
            )
            .await?;
        Ok(RemoteEntry {
            parent: Some(parent.clone()),
            mime: mime.map(str::to_string),
            validator: etag.map(Validator::Etag),
            ..RemoteEntry::file(id, name, 0)
        })
    }

    async fn write_content(
        &self,
        node: &NodeId,
        body: ContentSource,
        if_unchanged: Option<&Validator>,
    ) -> Result<RemoteEntry, RemoteError> {
        let len = match &body {
            ContentSource::Bytes(b) => b.len() as u64,
            ContentSource::File { len, .. } => *len,
        };
        let etag = self
            .client
            .put(
                Self::dav_path(node),
                body,
                if_unchanged.and_then(|v| v.as_http_etag()),
                None,
            )
            .await?;

        let name = node.as_str().rsplit('/').next().unwrap_or_default();
        Ok(RemoteEntry {
            parent: Self::parent_of(node),
            validator: etag.map(Validator::Etag),
            ..RemoteEntry::file(node.clone(), name, len)
        })
    }

    async fn remove(
        &self,
        node: &NodeId,
        if_unchanged: Option<&Validator>,
    ) -> Result<(), RemoteError> {
        self.client
            .delete(
                Self::dav_path(node),
                if_unchanged.and_then(|v| v.as_http_etag()),
            )
            .await
    }

    async fn rename(
        &self,
        node: &NodeId,
        new_parent: &NodeId,
        new_name: &str,
    ) -> Result<RemoteEntry, RemoteError> {
        let dest = Self::join(new_parent, new_name);
        self.client
            .move_to(
                Self::dav_path(node),
                &self.client.url_for_public(Self::dav_path(&dest)),
            )
            .await?;
        Ok(RemoteEntry {
            parent: Some(new_parent.clone()),
            ..RemoteEntry::file(dest, new_name, 0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(base: &str) -> WebDavClient {
        WebDavClient::new(base.to_string(), "u".into(), "p".into()).unwrap()
    }

    #[test]
    fn base_url_gains_a_trailing_slash() {
        // Without this, Url::join drops the last segment and every request goes
        // to the wrong place.
        let c = client("https://h/remote.php/dav/files/alice");
        assert_eq!(
            c.base_url().as_str(),
            "https://h/remote.php/dav/files/alice/"
        );

        let c = client("https://h/remote.php/dav/files/alice/");
        assert_eq!(
            c.base_url().as_str(),
            "https://h/remote.php/dav/files/alice/"
        );
    }

    #[test]
    fn url_for_preserves_the_base_path() {
        let c = client("https://h/remote.php/dav/files/alice");
        assert_eq!(
            c.url_for("Documents/report.pdf", false).as_str(),
            "https://h/remote.php/dav/files/alice/Documents/report.pdf"
        );
        // Leading slash must not reset to the server root -- Url::join would.
        assert_eq!(
            c.url_for("/Documents/report.pdf", false).as_str(),
            "https://h/remote.php/dav/files/alice/Documents/report.pdf"
        );
    }

    #[test]
    fn url_for_encodes_hostile_names() {
        let c = client("https://h/dav/");
        // '#' would otherwise start a fragment and truncate the path.
        assert_eq!(
            c.url_for("hash#and&.txt", false).as_str(),
            "https://h/dav/hash%23and&.txt"
        );
        // '?' would otherwise start a query string.
        assert!(c.url_for("what?.txt", false).as_str().contains("%3F"));
        assert_eq!(
            c.url_for("spaces in name.txt", false).as_str(),
            "https://h/dav/spaces%20in%20name.txt"
        );
        assert_eq!(
            c.url_for("ümlaut-Ä.txt", false).as_str(),
            "https://h/dav/%C3%BCmlaut-%C3%84.txt"
        );
        // A literal percent must be re-encoded, not treated as an escape.
        assert_eq!(
            c.url_for("100%done.txt", false).as_str(),
            "https://h/dav/100%25done.txt"
        );
    }

    #[test]
    fn directories_get_a_trailing_slash() {
        let c = client("https://h/dav/");
        assert_eq!(c.url_for("sub", true).as_str(), "https://h/dav/sub/");
        assert_eq!(c.url_for("", true).as_str(), "https://h/dav/");
    }

    #[test]
    fn url_round_trips_through_resolve() {
        // What url_for encodes, resolve must decode back to the same name.
        let c = client("https://h/dav/");
        for name in [
            "hash#and&.txt",
            "spaces in name.txt",
            "ümlaut-Ä.txt",
            "100%done.txt",
            "apostrophe's.txt",
            "plus+sign.txt",
        ] {
            let url = c.url_for(name, false);
            let responses = [crate::remote::propfind::PropfindResponse {
                href: url.path().to_string(),
                ..Default::default()
            }];
            let listing = resolve(c.base_url(), &responses);
            assert_eq!(listing.children.len(), 1, "{name}");
            assert_eq!(listing.children[0].name, name, "round trip failed");
        }
    }

    #[test]
    fn status_mapping_never_reports_enoent_for_a_server_problem() {
        let u = Url::parse("https://h/").unwrap();
        for s in [
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            let err = check_status(s, &u).unwrap_err();
            assert_ne!(err.errno(), libc::ENOENT, "{s} mapped to ENOENT");
        }
        assert!(matches!(
            check_status(StatusCode::UNAUTHORIZED, &u),
            Err(RemoteError::Unauthorized)
        ));
        assert!(matches!(
            check_status(StatusCode::NOT_FOUND, &u),
            Err(RemoteError::NotFound)
        ));
        assert!(matches!(
            check_status(StatusCode::PRECONDITION_FAILED, &u),
            Err(RemoteError::Changed)
        ));
        assert!(check_status(StatusCode::OK, &u).is_ok());
        assert!(check_status(StatusCode::MULTI_STATUS, &u).is_ok());
    }
}
