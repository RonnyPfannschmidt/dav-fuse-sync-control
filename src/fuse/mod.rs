//! The FUSE layer.
//!
//! Phase 6 moves `DavFS` here and reduces it to spawn-and-reply shims over
//! `Vfs`. For now it holds only the pure attribute helpers.

pub mod attr;

pub use attr::{to_file_attr, NodeMeta, OwnerIds};
