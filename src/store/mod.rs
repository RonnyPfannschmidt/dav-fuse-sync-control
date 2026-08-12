//! Local persistence: the metadata cache, and (from Phase 7) the content cache.

pub mod meta;

pub use meta::{CacheStats, CachedListing, MetaStore};
