//! davfs-sync: a FUSE WebDAV client with an offline metadata cache.
//!
//! Everything lives here rather than in the binary so that integration tests in
//! `tests/` can import it -- a binary target cannot be imported.

pub mod cli;
pub mod clock;
pub mod config;
pub mod creds;
pub mod error;
pub mod filesystem;
pub mod fuse;
pub mod remote;
pub mod secrets;
pub mod store;

pub use error::RemoteError;
pub use remote::{NodeId, RemoteEntry, RemoteStorage};

/// Compatibility path for the WebDAV client, which now lives behind the seam.
pub mod webdav {
    pub use crate::remote::propfind;
    pub use crate::remote::webdav::*;
}
