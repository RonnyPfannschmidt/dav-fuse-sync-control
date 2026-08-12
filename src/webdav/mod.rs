pub mod propfind;

use reqwest::{Client, StatusCode};
use std::time::Duration;
use url::Url;

use crate::error::RemoteError;
use propfind::{parse_multistatus, propfind_body, resolve, ResolvedListing};

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

    fn request(&self, method: &str, url: Url) -> reqwest::RequestBuilder {
        let method =
            reqwest::Method::from_bytes(method.as_bytes()).expect("method names here are literals");
        self.client
            .request(method, url)
            .basic_auth(&self.username, Some(&self.password))
    }
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
            let responses = [propfind::PropfindResponse {
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
