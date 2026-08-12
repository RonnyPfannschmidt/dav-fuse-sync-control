//! Building `FileAttr` from cached metadata.
//!
//! Pure and dependency-free so it can be unit tested. The PoC had three
//! near-identical builders that each hardcoded `UNIX_EPOCH` for every
//! timestamp, discarding the mtime it had already parsed and stored, and each
//! called `libc::getuid()` directly, which makes the result machine-dependent
//! and unsnapshotable.

use fuser::{FileAttr, FileType};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Ownership reported for every node. Injected rather than read from libc so
/// tests produce stable output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnerIds {
    pub uid: u32,
    pub gid: u32,
}

impl OwnerIds {
    /// The ids of the calling process.
    pub fn current() -> Self {
        // Safe: getuid/getgid cannot fail and take no arguments.
        Self {
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }
}

/// What the filesystem knows about one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeMeta {
    pub ino: u64,
    pub is_dir: bool,
    pub size: u64,
    /// Seconds since the Unix epoch, if the server told us.
    pub mtime: Option<i64>,
}

impl NodeMeta {
    pub fn dir(ino: u64) -> Self {
        Self {
            ino,
            is_dir: true,
            size: 0,
            mtime: None,
        }
    }

    pub fn file(ino: u64, size: u64) -> Self {
        Self {
            ino,
            is_dir: false,
            size,
            mtime: None,
        }
    }

    pub fn with_mtime(mut self, mtime: Option<i64>) -> Self {
        self.mtime = mtime;
        self
    }
}

/// Convert a Unix timestamp to a `SystemTime`, clamping rather than panicking.
///
/// Negative values (pre-1970) and values beyond `SystemTime`'s range are
/// clamped to the epoch: a nonsensical timestamp from a server must not be able
/// to take down the mount.
fn to_system_time(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH
            .checked_add(Duration::from_secs(secs as u64))
            .unwrap_or(UNIX_EPOCH)
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(secs.unsigned_abs()))
            .unwrap_or(UNIX_EPOCH)
    }
}

/// Build the kernel's view of a node.
pub fn to_file_attr(meta: &NodeMeta, owner: OwnerIds) -> FileAttr {
    let time = meta.mtime.map(to_system_time).unwrap_or(UNIX_EPOCH);

    FileAttr {
        ino: meta.ino,
        size: meta.size,
        blocks: meta.size.div_ceil(512),
        atime: time,
        mtime: time,
        ctime: time,
        crtime: time,
        kind: if meta.is_dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        },
        // Read-only for now; Phase 10 makes this depend on config.
        perm: if meta.is_dir { 0o755 } else { 0o644 },
        // 2 for a directory (itself and '.'); subdirectories would add more,
        // but nothing relies on an exact count and we do not know it cheaply.
        nlink: if meta.is_dir { 2 } else { 1 },
        uid: owner.uid,
        gid: owner.gid,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: OwnerIds = OwnerIds {
        uid: 1000,
        gid: 1000,
    };

    #[test]
    fn mtime_is_reported_not_discarded() {
        // The PoC hardcoded UNIX_EPOCH here, so every file looked like 1970.
        let meta = NodeMeta::file(42, 100).with_mtime(Some(1_600_000_000));
        let attr = to_file_attr(&meta, OWNER);

        let expected = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        assert_eq!(attr.mtime, expected);
        assert_eq!(attr.ctime, expected);
        assert_ne!(attr.mtime, UNIX_EPOCH);
    }

    #[test]
    fn missing_mtime_falls_back_to_epoch() {
        let attr = to_file_attr(&NodeMeta::file(1, 0), OWNER);
        assert_eq!(attr.mtime, UNIX_EPOCH);
    }

    #[test]
    fn extreme_timestamps_clamp_instead_of_panicking() {
        for secs in [i64::MIN, i64::MAX, -1] {
            let attr = to_file_attr(&NodeMeta::file(1, 0).with_mtime(Some(secs)), OWNER);
            let _ = attr.mtime; // must not have panicked
        }
    }

    #[test]
    fn block_count_rounds_up() {
        assert_eq!(to_file_attr(&NodeMeta::file(1, 0), OWNER).blocks, 0);
        assert_eq!(to_file_attr(&NodeMeta::file(1, 1), OWNER).blocks, 1);
        assert_eq!(to_file_attr(&NodeMeta::file(1, 512), OWNER).blocks, 1);
        assert_eq!(to_file_attr(&NodeMeta::file(1, 513), OWNER).blocks, 2);
    }

    #[test]
    fn kind_and_permissions_follow_is_dir() {
        let d = to_file_attr(&NodeMeta::dir(1), OWNER);
        assert_eq!(d.kind, FileType::Directory);
        assert_eq!(d.perm, 0o755);
        assert_eq!(d.nlink, 2);

        let f = to_file_attr(&NodeMeta::file(2, 9), OWNER);
        assert_eq!(f.kind, FileType::RegularFile);
        assert_eq!(f.perm, 0o644);
        assert_eq!(f.size, 9);
    }

    #[test]
    fn ownership_is_injected_not_read_from_the_process() {
        let attr = to_file_attr(&NodeMeta::file(1, 0), OwnerIds { uid: 7, gid: 9 });
        assert_eq!((attr.uid, attr.gid), (7, 9));
    }
}
