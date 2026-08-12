//! davfs-sync: a FUSE WebDAV client with an offline metadata cache.
//!
//! Everything lives here rather than in the binary so that integration tests in
//! `tests/` can import it -- a binary target cannot be imported.

pub mod cache;
pub mod cli;
pub mod config;
pub mod filesystem;
pub mod secrets;
pub mod webdav;
