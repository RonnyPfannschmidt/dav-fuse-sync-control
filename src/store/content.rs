//! The on-disk content cache.
//!
//! Blobs are keyed by **(node identity, validator)**, never by inode or path.
//! A changed file therefore gets a different key, so a stale blob is
//! structurally unservable -- there is no window in which the wrong bytes can
//! be returned. Reverting a file to an earlier version is a cache *hit*, if
//! that blob survived eviction.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use tokio::io::AsyncWriteExt;

use crate::error::RemoteError;
use crate::remote::{NodeId, Validator};

/// Two hex characters of sharding, so a large mount does not put a hundred
/// thousand files in one directory.
const SHARD_LEN: usize = 2;

pub struct ContentCache {
    root: PathBuf,
}

impl ContentCache {
    /// `root` gains `blobs/` and `partial/` subdirectories.
    ///
    /// They are siblings deliberately: publishing a blob is a `rename`, which
    /// is only atomic within one filesystem.
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("blobs"))?;
        std::fs::create_dir_all(root.join("partial"))?;
        Ok(Self { root })
    }

    /// The cache key for a node at a particular version.
    ///
    /// Because [`Validator::SizeMtime`] is the floor, a key almost always
    /// exists. When a backend can offer nothing at all the node is treated as
    /// uncacheable rather than cached under an ambiguous key.
    pub fn key_for(id: &NodeId, validator: Option<&Validator>) -> Option<String> {
        let v = validator?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(id.as_str().as_bytes());
        hasher.update(&[0]);
        hasher.update(&[v.kind()]);
        hasher.update(&[0]);
        hasher.update(v.value().as_bytes());
        Some(hasher.finalize().to_hex().to_string())
    }

    fn shard(key: &str) -> &str {
        &key[..SHARD_LEN.min(key.len())]
    }

    pub fn blob_path(&self, key: &str) -> PathBuf {
        self.root.join("blobs").join(Self::shard(key)).join(key)
    }

    fn partial_path(&self, key: &str) -> PathBuf {
        self.root
            .join("partial")
            .join(Self::shard(key))
            .join(format!("{key}.part"))
    }

    pub fn has(&self, key: &str) -> bool {
        self.blob_path(key).is_file()
    }

    /// Open a blob for reading.
    ///
    /// The caller holds the returned handle for the lifetime of the FUSE file
    /// handle, which is what makes eviction safe: unlinking a blob out from
    /// under an open descriptor is harmless on Linux.
    pub fn open(&self, key: &str) -> std::io::Result<std::fs::File> {
        std::fs::File::open(self.blob_path(key))
    }

    pub fn len_of(&self, key: &str) -> Option<u64> {
        std::fs::metadata(self.blob_path(key)).ok().map(|m| m.len())
    }

    /// Write a stream to a blob, atomically.
    ///
    /// Content goes to `partial/`, is fsync'd, and only then renamed into
    /// `blobs/`. The caller must not record the key in the database until this
    /// returns, so the database can never advertise a blob that is not fully
    /// present.
    pub async fn store<S>(&self, key: &str, mut stream: S) -> Result<u64, RemoteError>
    where
        S: Stream<Item = Result<Bytes, RemoteError>> + Unpin,
    {
        let partial = self.partial_path(key);
        let blob = self.blob_path(key);
        if let Some(parent) = partial.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if let Some(parent) = blob.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut written = 0u64;
        {
            let mut file = tokio::fs::File::create(&partial).await?;
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        // Leave nothing half-written behind on failure.
                        let _ = tokio::fs::remove_file(&partial).await;
                        return Err(e);
                    }
                };
                file.write_all(&chunk).await?;
                written += chunk.len() as u64;
            }
            file.flush().await?;
            // Durable before the rename, or a crash can publish a truncated blob.
            file.sync_all().await?;
        }

        tokio::fs::rename(&partial, &blob).await?;
        Ok(written)
    }

    /// Remove a blob. Always called *after* the database has stopped pointing
    /// at it.
    pub fn remove(&self, key: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.blob_path(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Delete leftover `.part` files from an interrupted run.
    ///
    /// Called at startup, before any download can begin.
    pub fn sweep_partials(&self) -> std::io::Result<usize> {
        let mut removed = 0;
        let partial_root = self.root.join("partial");
        if !partial_root.is_dir() {
            return Ok(0);
        }
        for shard in std::fs::read_dir(&partial_root)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let entry = entry?;
                if entry.path().extension().and_then(|e| e.to_str()) == Some("part") {
                    std::fs::remove_file(entry.path())?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// Blob keys present on disk, for reconciling against the database.
    pub fn blob_keys(&self) -> std::io::Result<Vec<String>> {
        let mut keys = Vec::new();
        let blobs = self.root.join("blobs");
        if !blobs.is_dir() {
            return Ok(keys);
        }
        for shard in std::fs::read_dir(&blobs)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let entry = entry?;
                if let Some(name) = entry.file_name().to_str() {
                    keys.push(name.to_string());
                }
            }
        }
        Ok(keys)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::Validator;

    fn cache() -> (tempfile::TempDir, ContentCache) {
        let dir = tempfile::tempdir().unwrap();
        let c = ContentCache::new(dir.path()).unwrap();
        (dir, c)
    }

    fn stream_of(data: &'static [u8]) -> impl Stream<Item = Result<Bytes, RemoteError>> + Unpin {
        Box::pin(futures::stream::once(async move {
            Ok(Bytes::from_static(data))
        }))
    }

    #[test]
    fn the_key_changes_when_the_validator_does() {
        let id = NodeId("/a.txt".into());
        let v1 = Validator::Etag("\"1\"".into());
        let v2 = Validator::Etag("\"2\"".into());

        let k1 = ContentCache::key_for(&id, Some(&v1)).unwrap();
        let k2 = ContentCache::key_for(&id, Some(&v2)).unwrap();
        assert_ne!(k1, k2, "a changed file must not reuse its blob");

        // ...and is stable for the same inputs.
        assert_eq!(k1, ContentCache::key_for(&id, Some(&v1)).unwrap());
    }

    #[test]
    fn different_nodes_with_the_same_validator_do_not_collide() {
        let v = Validator::SizeMtime {
            size: 10,
            mtime_unix: 20,
        };
        let a = ContentCache::key_for(&NodeId("/a".into()), Some(&v)).unwrap();
        let b = ContentCache::key_for(&NodeId("/b".into()), Some(&v)).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn a_node_with_no_validator_is_uncacheable() {
        // Better than caching under an ambiguous key and serving stale bytes.
        assert_eq!(ContentCache::key_for(&NodeId("/a".into()), None), None);
    }

    #[tokio::test]
    async fn store_publishes_atomically_and_leaves_no_partial() {
        let (_d, c) = cache();
        let n = c
            .store("abcdef123", stream_of(b"hello world"))
            .await
            .unwrap();

        assert_eq!(n, 11);
        assert!(c.has("abcdef123"));
        assert_eq!(c.len_of("abcdef123"), Some(11));
        assert_eq!(
            c.sweep_partials().unwrap(),
            0,
            "a .part file was left behind"
        );

        let mut buf = String::new();
        use std::io::Read;
        c.open("abcdef123")
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, "hello world");
    }

    #[tokio::test]
    async fn a_failed_download_publishes_nothing() {
        let (_d, c) = cache();
        let failing = Box::pin(futures::stream::iter(vec![
            Ok(Bytes::from_static(b"partial data")),
            Err(RemoteError::Offline("cut".into())),
        ]));

        let err = c.store("deadbeef", failing).await.unwrap_err();
        assert_eq!(err.errno(), libc::ENETUNREACH);
        assert!(
            !c.has("deadbeef"),
            "a truncated blob was published as if complete"
        );
        assert_eq!(
            c.sweep_partials().unwrap(),
            0,
            "the .part was not cleaned up"
        );
    }

    #[tokio::test]
    async fn an_open_blob_survives_eviction() {
        // This is what makes eviction safe while readers are active.
        let (_d, c) = cache();
        c.store("cafe0001", stream_of(b"content")).await.unwrap();

        let mut handle = c.open("cafe0001").unwrap();
        c.remove("cafe0001").unwrap();
        assert!(!c.has("cafe0001"));

        use std::io::Read;
        let mut buf = String::new();
        handle.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "content", "unlinking broke an open reader");
    }

    #[tokio::test]
    async fn sweep_removes_orphaned_partials() {
        let (_d, c) = cache();
        let stray = c.root().join("partial").join("ab");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("abcdef.part"), b"interrupted").unwrap();

        assert_eq!(c.sweep_partials().unwrap(), 1);
        assert_eq!(c.sweep_partials().unwrap(), 0);
    }

    #[tokio::test]
    async fn blob_keys_reports_what_is_on_disk() {
        let (_d, c) = cache();
        c.store("aa11", stream_of(b"x")).await.unwrap();
        c.store("bb22", stream_of(b"y")).await.unwrap();

        let mut keys = c.blob_keys().unwrap();
        keys.sort();
        assert_eq!(keys, vec!["aa11", "bb22"]);
    }

    #[test]
    fn removing_an_absent_blob_is_not_an_error() {
        let (_d, c) = cache();
        c.remove("never-existed").unwrap();
    }
}
