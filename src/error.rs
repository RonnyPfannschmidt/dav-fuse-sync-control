//! Typed errors for remote storage backends.
//!
//! The PoC returned `anyhow::Error` from every backend call and the FUSE layer
//! mapped all of them to `ENOENT`, so a rate-limit response surfaced to
//! applications as "this file does not exist". That is not merely unhelpful --
//! rsync and several file managers treat `ENOENT` as permission to delete.
//!
//! Every variant here maps to exactly one errno, and the mapping is tested.

use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// Credentials were rejected. Never retried: repeated 401s lock out
    /// Nextcloud accounts via its brute-force protection.
    #[error("authentication failed")]
    Unauthorized,

    /// Authenticated, but not permitted.
    #[error("permission denied")]
    Forbidden,

    /// The access token expired and should be refreshed before retrying.
    /// Unused by WebDAV; the Google Drive backend needs it.
    #[error("credentials expired")]
    AuthExpired,

    #[error("not found")]
    NotFound,

    #[error("already exists")]
    Exists,

    /// A precondition failed -- the remote changed under us (HTTP 412).
    #[error("remote changed under us")]
    Changed,

    #[error("resource is locked")]
    Locked,

    /// HTTP 429, or 503 with a Retry-After.
    #[error("rate limited")]
    RateLimited(Option<Duration>),

    #[error("insufficient remote storage")]
    QuotaExceeded,

    /// DNS failure, connection refused, or timeout. Distinct from a server
    /// error: this is the state in which serving stale cache is correct.
    #[error("offline: {0}")]
    Offline(String),

    #[error("server error {status}")]
    Server { status: u16, body: String },

    /// The response was not something we could parse.
    #[error("malformed response: {0}")]
    Protocol(String),

    /// The backend does not implement this operation.
    #[error("unsupported operation: {0}")]
    Unsupported(&'static str),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl RemoteError {
    /// The errno this error should surface to the kernel as.
    pub fn errno(&self) -> i32 {
        use RemoteError::*;
        match self {
            // EACCES rather than EPERM: the caller may be able to fix this by
            // re-authenticating, which is what EACCES conventionally implies.
            Unauthorized | Forbidden | AuthExpired => libc::EACCES,
            NotFound => libc::ENOENT,
            Exists => libc::EEXIST,
            // ESTALE tells the kernel to drop its cached handle and retry.
            Changed => libc::ESTALE,
            Locked | RateLimited(_) => libc::EAGAIN,
            QuotaExceeded => libc::EDQUOT,
            Offline(_) => libc::ENETUNREACH,
            Server { .. } | Protocol(_) => libc::EIO,
            // EROFS, never ENOSYS: ENOSYS from a FUSE op makes the kernel
            // permanently disable that operation for the lifetime of the mount.
            Unsupported(_) => libc::EROFS,
            Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
        }
    }

    /// The same mapping as [`Self::errno`], in the form fuser's reply types
    /// want.
    pub fn to_errno(&self) -> fuser::Errno {
        fuser::Errno::from_i32(self.errno())
    }

    /// Whether retrying with backoff could plausibly succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            RemoteError::RateLimited(_)
                | RemoteError::Server { .. }
                | RemoteError::Offline(_)
                | RemoteError::Io(_)
        )
    }

    /// How long the server asked us to wait, if it said.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            RemoteError::RateLimited(d) => *d,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_is_never_enoent() {
        // The whole point of the type: a network failure must not look like a
        // missing file, or rsync and file managers will delete things.
        assert_eq!(
            RemoteError::Offline("dns".into()).errno(),
            libc::ENETUNREACH
        );
        assert_ne!(RemoteError::Offline("dns".into()).errno(), libc::ENOENT);
        assert_ne!(RemoteError::RateLimited(None).errno(), libc::ENOENT);
        assert_ne!(
            RemoteError::Server {
                status: 500,
                body: String::new()
            }
            .errno(),
            libc::ENOENT
        );
    }

    #[test]
    fn unsupported_is_erofs_not_enosys() {
        // ENOSYS would make the kernel permanently disable the op for the
        // lifetime of the mount, silently poisoning a later read-write rollout.
        assert_eq!(RemoteError::Unsupported("write").errno(), libc::EROFS);
        assert_ne!(RemoteError::Unsupported("write").errno(), libc::ENOSYS);
    }

    #[test]
    fn auth_failures_are_not_retryable() {
        assert!(!RemoteError::Unauthorized.is_retryable());
        assert!(!RemoteError::Forbidden.is_retryable());
        assert!(!RemoteError::NotFound.is_retryable());
        assert!(RemoteError::RateLimited(None).is_retryable());
        assert!(RemoteError::Offline("x".into()).is_retryable());
    }

    #[test]
    fn retry_after_is_carried() {
        let e = RemoteError::RateLimited(Some(Duration::from_secs(30)));
        assert_eq!(e.retry_after(), Some(Duration::from_secs(30)));
        assert_eq!(RemoteError::Unauthorized.retry_after(), None);
    }
}
