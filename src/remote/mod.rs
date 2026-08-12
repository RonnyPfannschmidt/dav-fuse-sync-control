//! The storage backend seam.
//!
//! Everything above this layer -- the cache, the FUSE handlers, the sync worker
//! -- talks to [`RemoteStorage`] and never to WebDAV directly.
//!
//! # Why the types look like this
//!
//! The seam is shaped so a Google Drive backend can drop in without changing
//! it. Drive differs from WebDAV in five ways that are expensive to retrofit,
//! and each one is why some type here is less obvious than it could be:
//!
//! | Drive property | Consequence here |
//! |---|---|
//! | files are addressed by opaque id, not path | [`NodeId`] is opaque; [`RemoteStorage::root`] is a method, not the constant `"/"` |
//! | two files in a folder may share a name | [`Capabilities::unique_names`]; the cache may not put a UNIQUE index on `(parent, name)` |
//! | there is no per-directory ETag | [`Validator`] has four shapes, and [`Capabilities::dir_validators`] says whether one exists at all |
//! | change detection is one account-global cursor | [`ChangeFeed`] and [`RemoteStorage::poll_changes`] |
//! | Google-native docs have no size until exported | [`SizeHint::Unknown`] -- a bare `u64` here would be a lie |
//!
//! WebDAV pays nothing for any of these: it reports `path_addressed: true`,
//! `unique_names: true`, `dir_validators: true`, and `ChangeFeed::PerDirectory`.

pub mod gdrive;
pub mod propfind;
pub mod webdav;

#[cfg(any(test, feature = "testing"))]
pub mod mock;

use bytes::Bytes;
use futures::Stream;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use crate::error::RemoteError;

pub use gdrive::GDriveBackend;
pub use webdav::WebDavBackend;

/// An opaque handle for a remote node, stable for that node's lifetime.
///
/// * WebDAV: the percent-decoded path below the account root. Always a leading
///   `/`, never a trailing one; the root is exactly `"/"`.
/// * Google Drive: the file id, e.g. `"1AbC..."`.
///
/// Stored as SQLite `TEXT`, so it must be valid UTF-8 and reasonably short.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct NodeId(pub String);

impl NodeId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for NodeId {
    fn from(s: &str) -> Self {
        NodeId(s.to_string())
    }
}

/// What a backend can and cannot do.
///
/// Read by the cache and the VFS to decide what is safe, so that neither has to
/// name a specific backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// A [`NodeId`] is a pure function of the path, so an id can be synthesized
    /// for a path that has never been listed. False for Drive, where ids only
    /// ever come from the server -- which is why `lookup` is a trait method
    /// rather than "join the path yourself".
    pub path_addressed: bool,

    /// Names are unique within a directory.
    ///
    /// False for Drive. The cache must therefore not put a UNIQUE constraint on
    /// `(parent_id, name)`, and the VFS must disambiguate before showing names
    /// to the kernel.
    pub unique_names: bool,

    /// A listing carries a validator usable for cheap revalidation.
    ///
    /// False for Drive: there is no per-folder ETag, so freshness comes from
    /// the change feed instead.
    pub dir_validators: bool,

    /// `GET` honours `Range:`.
    pub ranged_reads: bool,

    /// Bytes to fetch per request when streaming rather than downloading whole
    /// files. `None` means "no preference; whole-file is fine" (WebDAV).
    /// Drive wants 8-16 MiB, because each request costs quota.
    pub preferred_chunk: Option<u32>,

    pub max_upload_size: Option<u64>,

    /// The server is believed to enforce `If-Match` / `If-None-Match` on
    /// writes.
    ///
    /// **Best-effort, and not detectable cheaply.** Verified against
    /// `rclone serve webdav`, which returns 201/204 and performs the operation
    /// anyway for all three of `If-Match` on PUT, `If-None-Match: *` on an
    /// existing file, and `If-Match` on DELETE. Nextcloud and other SabreDAV
    /// servers do implement RFC 4918 `If` handling.
    ///
    /// The consequence for the future conflict policy is the point: passing
    /// `if_unchanged` is necessary but **not sufficient** to prevent a lost
    /// update. Where correctness actually matters the caller must re-read and
    /// compare after writing, rather than trusting the precondition.
    pub enforces_preconditions: bool,
}

impl Capabilities {
    /// Sensible conservative defaults: assume nothing beyond path addressing.
    pub fn minimal() -> Self {
        Self {
            path_addressed: true,
            unique_names: true,
            dir_validators: false,
            ranged_reads: false,
            preferred_chunk: None,
            max_upload_size: None,
            enforces_preconditions: false,
        }
    }
}

/// How the backend tells us something changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeFeed {
    /// Revalidate each directory independently (WebDAV).
    PerDirectory,
    /// One account-global cursor; [`RemoteStorage::poll_changes`] returns
    /// everything that moved (Google Drive's `changes.list`).
    ///
    /// Structurally different from `PerDirectory`: a delta arrives as a file id
    /// with no indication of which cached directory it belongs to, and a *move*
    /// must invalidate both the old and the new parent. That is why the cache
    /// is a flat id-keyed table with a `parent_id` rather than a per-directory
    /// child list.
    Cursor,
}

/// Evidence that a node or listing has or has not changed.
///
/// Four shapes because no single one is available everywhere.
/// [`Validator::SizeMtime`] is the floor, so `Option<Validator>` is `None` only
/// when the backend genuinely cannot say anything -- which in turn means the
/// content-cache key can always be computed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Validator {
    /// HTTP entity tag (WebDAV `getetag`, Drive `headRevisionId`).
    Etag(String),
    /// Monotonic counter (Drive `version`).
    Version(i64),
    /// Content hash (Drive `md5Checksum`, Nextcloud `oc:checksums`).
    Checksum { algo: ChecksumAlgo, hex: String },
    /// The floor: everyone can report these two.
    SizeMtime { size: u64, mtime_unix: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChecksumAlgo {
    Md5,
    Sha1,
    Sha256,
}

impl Validator {
    /// A short tag identifying the variant, for the content-cache key and the
    /// `validator_kind` column.
    pub fn kind(&self) -> u8 {
        match self {
            Validator::Etag(_) => 0,
            Validator::Version(_) => 1,
            Validator::Checksum { .. } => 2,
            Validator::SizeMtime { .. } => 3,
        }
    }

    /// The variant's payload, for the same purposes.
    pub fn value(&self) -> String {
        match self {
            Validator::Etag(e) => e.clone(),
            Validator::Version(v) => v.to_string(),
            Validator::Checksum { hex, .. } => hex.clone(),
            Validator::SizeMtime { size, mtime_unix } => format!("{size}:{mtime_unix}"),
        }
    }

    /// The `If-None-Match` / `If-Match` header value, when the validator is an
    /// entity tag. Other shapes are compared locally instead.
    pub fn as_http_etag(&self) -> Option<&str> {
        match self {
            Validator::Etag(e) => Some(e),
            _ => None,
        }
    }
}

/// How big a node is.
///
/// Not a `u64`: Google-native documents (Docs, Sheets, Slides) have no size
/// until they are exported, and reporting `0` is what makes them unreadable
/// through a FUSE mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SizeHint {
    Exact(u64),
    Unknown,
}

impl SizeHint {
    /// A size to report in `stat` before the true one is known.
    ///
    /// Never 0: readers stop at the first byte and conclude the file is empty.
    pub const PLACEHOLDER: u64 = 4 * 1024 * 1024;

    pub fn or_placeholder(self) -> u64 {
        match self {
            SizeHint::Exact(n) => n,
            SizeHint::Unknown => Self::PLACEHOLDER,
        }
    }

    pub fn exact(self) -> Option<u64> {
        match self {
            SizeHint::Exact(n) => Some(n),
            SizeHint::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NodeKind {
    File,
    Dir,
    Symlink,
    /// Content exists only via an export step and has no byte size until then
    /// (Google Docs). Read like a file, but the size is a [`SizeHint::Unknown`]
    /// until the first export completes.
    ExportOnly,
    Other,
}

impl NodeKind {
    pub fn is_dir(self) -> bool {
        matches!(self, NodeKind::Dir)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RemoteFlags {
    pub read_only: bool,
    pub trashed: bool,
    /// A Drive shortcut; `RemoteEntry::target` holds what it points at.
    pub shortcut: bool,
}

/// One node's metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub id: NodeId,
    /// `None` only for the account root.
    pub parent: Option<NodeId>,
    /// The name as the backend stores it.
    ///
    /// May contain characters a filesystem cannot represent -- Drive permits
    /// `/`, `.` and `..`. Sanitising and disambiguating is the cache's job, not
    /// the backend's, so that every backend gets it for free.
    pub name: String,
    pub kind: NodeKind,
    pub size: SizeHint,
    pub mtime: Option<chrono::DateTime<chrono::Utc>>,
    pub mime: Option<String>,
    pub validator: Option<Validator>,
    pub flags: RemoteFlags,
    /// For shortcuts: the node this points at.
    pub target: Option<NodeId>,
}

impl RemoteEntry {
    /// A plain file entry, for backends and tests that need nothing exotic.
    pub fn file(id: impl Into<NodeId>, name: impl Into<String>, size: u64) -> Self {
        Self {
            id: id.into(),
            parent: None,
            name: name.into(),
            kind: NodeKind::File,
            size: SizeHint::Exact(size),
            mtime: None,
            mime: None,
            validator: None,
            flags: RemoteFlags::default(),
            target: None,
        }
    }

    pub fn dir(id: impl Into<NodeId>, name: impl Into<String>) -> Self {
        Self {
            kind: NodeKind::Dir,
            size: SizeHint::Exact(0),
            ..Self::file(id, name, 0)
        }
    }
}

impl From<String> for NodeId {
    fn from(s: String) -> Self {
        NodeId(s)
    }
}

/// The result of listing a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    /// Nothing changed; the caller's cached children remain valid.
    ///
    /// A distinct variant rather than an empty `Vec`, which cannot be told
    /// apart from "the directory is now empty".
    Unchanged,
    Fresh {
        /// Validator for the *listing*, echoed back as `if_unchanged` next time.
        /// `None` when [`Capabilities::dir_validators`] is false.
        validator: Option<Validator>,
        /// The directory's own entry. WebDAV's `Depth: 1` returns it, and it is
        /// where the directory validator comes from -- but it must never be
        /// listed as one of its own children.
        ///
        /// Boxed: `RemoteEntry` is much larger than the `Unchanged` variant,
        /// and every listing pays for the difference otherwise.
        this: Option<Box<RemoteEntry>>,
        children: Vec<RemoteEntry>,
        /// False when this is a partial page and more remain (Drive paginates
        /// at 1000). The cache must not delete unlisted children when false, or
        /// a large directory loses everything past the first page.
        complete: bool,
    },
}

/// A batch of changes from a cursor backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeBatch {
    pub cursor: String,
    pub changes: Vec<Change>,
    /// More pages are available right now.
    pub more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Upserted(Box<RemoteEntry>),
    Removed(NodeId),
    /// The backend lost the thread (an expired or invalid cursor). Drop all
    /// cached metadata and re-enumerate.
    Resync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub offset: u64,
    /// `None` means "to the end".
    pub len: Option<u64>,
}

/// A content download in progress.
///
/// Manually `Debug` (the body is a stream) so `Result<ContentStream, _>` can be
/// unwrapped in tests.
pub struct ContentStream {
    /// The validator the bytes were actually fetched at. Verify against
    /// expectations before caching, or two versions can be spliced together.
    pub validator: Option<Validator>,
    /// Total size, from `Content-Range` when ranged, else `Content-Length`.
    pub total_size: Option<u64>,
    pub body: Pin<Box<dyn Stream<Item = Result<Bytes, RemoteError>> + Send>>,
}

impl std::fmt::Debug for ContentStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentStream")
            .field("validator", &self.validator)
            .field("total_size", &self.total_size)
            .field("body", &"<stream>")
            .finish()
    }
}

/// The body of an upload.
pub enum ContentSource {
    Bytes(Bytes),
    /// Stream straight from a cache blob rather than loading it into memory.
    File {
        path: std::path::PathBuf,
        len: u64,
    },
}

/// A storage backend.
///
/// Object-safe (`Arc<dyn RemoteStorage>`), which is why it uses `async_trait`
/// rather than native `async fn` in traits.
///
/// The write half is declared here but **is not called from the FUSE layer**.
/// It exists so the signatures are validated against a real implementation
/// before a second backend has to conform to them -- in particular
/// `if_unchanged`, which needs to have been proven against a real 412 before
/// any conflict policy is built on it.
#[async_trait::async_trait]
pub trait RemoteStorage: Send + Sync + 'static {
    fn capabilities(&self) -> &Capabilities;

    fn change_feed(&self) -> ChangeFeed;

    /// Identity of the account root. A method, not a constant: Drive's root is
    /// `"root"`, and a multi-drive mount needs a synthesized one.
    fn root(&self) -> NodeId;

    /// The id of a child, if it can be derived without asking the server.
    ///
    /// `Some` for path-addressed backends, `None` otherwise. Lets the VFS
    /// resolve a path it has never listed; when `None` it must call
    /// [`Self::lookup`] instead.
    fn child_id(&self, _parent: &NodeId, _name: &str) -> Option<NodeId> {
        None
    }

    /// Cheap liveness and credentials probe. Drives readiness and reconnect.
    async fn probe(&self) -> Result<(), RemoteError>;

    /// List the children of `dir`.
    ///
    /// `if_unchanged` is the validator the caller holds for this directory's
    /// *listing*. If the backend can prove nothing moved it returns
    /// [`Listing::Unchanged`].
    async fn list(
        &self,
        dir: &NodeId,
        if_unchanged: Option<&Validator>,
    ) -> Result<Listing, RemoteError>;

    /// Metadata for one node, without listing its parent.
    async fn stat(&self, node: &NodeId) -> Result<RemoteEntry, RemoteError>;

    /// Resolve one child by name.
    ///
    /// The default lists the parent and scans, which is correct everywhere --
    /// including where names collide, since it returns the first match and
    /// disambiguation is a layer above. Path-addressed backends override it
    /// with a single cheap request.
    async fn lookup(&self, parent: &NodeId, name: &str) -> Result<RemoteEntry, RemoteError> {
        match self.list(parent, None).await? {
            Listing::Fresh { children, .. } => children
                .into_iter()
                .find(|c| c.name == name)
                .ok_or(RemoteError::NotFound),
            // Only returned when a validator was supplied, and none was.
            Listing::Unchanged => Err(RemoteError::NotFound),
        }
    }

    /// Stream a node's content.
    ///
    /// `if_unchanged` makes the read fail with [`RemoteError::Changed`] rather
    /// than silently splicing two versions of a file together.
    ///
    /// Honoured only when the server implements conditional requests; see
    /// [`Capabilities::enforces_preconditions`].
    async fn open_read(
        &self,
        node: &NodeId,
        range: Option<ByteRange>,
        if_unchanged: Option<&Validator>,
    ) -> Result<ContentStream, RemoteError>;

    // ---- cursor backends only -------------------------------------------

    /// A cursor for "everything from now on".
    async fn start_cursor(&self) -> Result<String, RemoteError> {
        Err(RemoteError::Unsupported("start_cursor"))
    }

    /// Changes since `cursor`.
    async fn poll_changes(&self, _cursor: &str) -> Result<ChangeBatch, RemoteError> {
        Err(RemoteError::Unsupported("poll_changes"))
    }

    // ---- write half: implemented, but not wired to FUSE ------------------

    async fn create_dir(&self, _parent: &NodeId, _name: &str) -> Result<RemoteEntry, RemoteError> {
        Err(RemoteError::Unsupported("create_dir"))
    }

    async fn create_file(
        &self,
        _parent: &NodeId,
        _name: &str,
        _mime: Option<&str>,
    ) -> Result<RemoteEntry, RemoteError> {
        Err(RemoteError::Unsupported("create_file"))
    }

    async fn write_content(
        &self,
        _node: &NodeId,
        _body: ContentSource,
        _if_unchanged: Option<&Validator>,
    ) -> Result<RemoteEntry, RemoteError> {
        Err(RemoteError::Unsupported("write_content"))
    }

    async fn remove(
        &self,
        _node: &NodeId,
        _if_unchanged: Option<&Validator>,
    ) -> Result<(), RemoteError> {
        Err(RemoteError::Unsupported("remove"))
    }

    async fn rename(
        &self,
        _node: &NodeId,
        _new_parent: &NodeId,
        _new_name: &str,
    ) -> Result<RemoteEntry, RemoteError> {
        Err(RemoteError::Unsupported("rename"))
    }

    async fn set_mtime(
        &self,
        _node: &NodeId,
        _mtime: SystemTime,
    ) -> Result<RemoteEntry, RemoteError> {
        Err(RemoteError::Unsupported("set_mtime"))
    }
}

// ---------------------------------------------------------------------------
// Retry
// ---------------------------------------------------------------------------

/// Retry policy for transient failures.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base: Duration::from_millis(200),
            max_backoff: Duration::from_secs(32),
        }
    }
}

impl RetryPolicy {
    pub fn none() -> Self {
        Self {
            max_attempts: 1,
            ..Self::default()
        }
    }

    /// How long to wait before attempt `attempt` (1-based), given the error.
    ///
    /// `Retry-After` wins when the server sent one. Otherwise exponential
    /// backoff with *full* jitter -- `rand(0, 2^n * base)` rather than
    /// `2^n * base ± noise` -- because full jitter is what actually breaks up
    /// the synchronised retry storm when many requests fail together.
    pub fn delay_for(&self, attempt: u32, err: &RemoteError) -> Duration {
        if let Some(after) = err.retry_after() {
            return after.min(self.max_backoff);
        }
        let exp = self
            .base
            .saturating_mul(1u32 << attempt.min(16))
            .min(self.max_backoff);
        let jitter: f64 = rand::random::<f64>();
        exp.mul_f64(jitter)
    }
}

/// Run `op`, retrying transient failures.
///
/// Only [`RemoteError::is_retryable`] errors are retried. A 401 is never
/// retried: repeated failed authentications trip Nextcloud's brute-force
/// protection and lock the account out.
pub async fn with_retry<T, F, Fut>(policy: RetryPolicy, mut op: F) -> Result<T, RemoteError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, RemoteError>>,
{
    let mut attempt = 0;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                attempt += 1;
                if attempt >= policy.max_attempts || !e.is_retryable() {
                    return Err(e);
                }
                let delay = policy.delay_for(attempt, &e);
                tracing::debug!(
                    "attempt {}/{} failed ({}); retrying in {:?}",
                    attempt,
                    policy.max_attempts,
                    e,
                    delay
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_hint_never_reports_zero_for_unknown() {
        // Reporting 0 is what makes Google Docs unreadable through rclone's
        // mount; readers stop at the first byte.
        assert_eq!(SizeHint::Unknown.or_placeholder(), SizeHint::PLACEHOLDER);
        assert_ne!(SizeHint::Unknown.or_placeholder(), 0);
        assert_eq!(SizeHint::Exact(42).or_placeholder(), 42);
        assert_eq!(SizeHint::Unknown.exact(), None);
    }

    #[test]
    fn validator_kinds_are_distinct_and_stable() {
        // These become a column value and part of the content-cache key, so
        // they must not be renumbered casually.
        assert_eq!(Validator::Etag("x".into()).kind(), 0);
        assert_eq!(Validator::Version(1).kind(), 1);
        assert_eq!(
            Validator::Checksum {
                algo: ChecksumAlgo::Md5,
                hex: "d4".into()
            }
            .kind(),
            2
        );
        assert_eq!(
            Validator::SizeMtime {
                size: 1,
                mtime_unix: 2
            }
            .kind(),
            3
        );
    }

    #[test]
    fn only_etags_go_into_http_headers() {
        assert_eq!(
            Validator::Etag("\"a\"".into()).as_http_etag(),
            Some("\"a\"")
        );
        assert_eq!(Validator::Version(3).as_http_etag(), None);
        // ...but every validator still has a value for local comparison.
        assert_eq!(Validator::Version(3).value(), "3");
        assert_eq!(
            Validator::SizeMtime {
                size: 10,
                mtime_unix: 20
            }
            .value(),
            "10:20"
        );
    }

    #[tokio::test]
    async fn retry_gives_up_immediately_on_auth_failure() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = AtomicU32::new(0);

        let r: Result<(), _> = with_retry(RetryPolicy::default(), || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(RemoteError::Unauthorized) }
        })
        .await;

        assert!(r.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a 401 must not be retried: it trips brute-force protection"
        );
    }

    #[tokio::test]
    async fn retry_retries_transient_failures_then_succeeds() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = AtomicU32::new(0);

        let policy = RetryPolicy {
            max_attempts: 4,
            base: Duration::from_millis(1),
            max_backoff: Duration::from_millis(5),
        };
        let r = with_retry(policy, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(RemoteError::Server {
                        status: 503,
                        body: String::new(),
                    })
                } else {
                    Ok(n)
                }
            }
        })
        .await;

        assert_eq!(r.unwrap(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_stops_at_max_attempts() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = AtomicU32::new(0);
        let policy = RetryPolicy {
            max_attempts: 3,
            base: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
        };

        let r: Result<(), _> = with_retry(policy, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(RemoteError::Offline("down".into())) }
        })
        .await;

        assert!(r.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn retry_after_overrides_backoff_and_is_capped() {
        let p = RetryPolicy::default();
        let e = RemoteError::RateLimited(Some(Duration::from_secs(5)));
        assert_eq!(p.delay_for(1, &e), Duration::from_secs(5));

        // A server asking for an hour must not park the filesystem for an hour.
        let e = RemoteError::RateLimited(Some(Duration::from_secs(3600)));
        assert_eq!(p.delay_for(1, &e), p.max_backoff);
    }

    #[test]
    fn backoff_is_bounded_and_jittered() {
        let p = RetryPolicy::default();
        let e = RemoteError::Server {
            status: 500,
            body: String::new(),
        };
        for attempt in 1..20 {
            let d = p.delay_for(attempt, &e);
            assert!(d <= p.max_backoff, "attempt {attempt} exceeded max backoff");
        }
        // Full jitter: repeated draws at the same attempt must differ.
        let draws: Vec<_> = (0..20).map(|_| p.delay_for(6, &e)).collect();
        assert!(
            draws.iter().any(|d| *d != draws[0]),
            "no jitter: synchronised clients would retry in lockstep"
        );
    }
}
