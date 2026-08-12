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
pub mod secrets;
pub mod store;
pub mod webdav;

pub use error::RemoteError;
