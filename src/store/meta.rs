//! The metadata cache: a flat, id-keyed node store.
//!
//! Keyed by [`NodeId`] rather than by path, with a single nullable `parent_id`.
//! See `migrations/003_node_store.sql` for why.

use anyhow::Result;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous},
    Row, SqlitePool,
};
use std::str::FromStr;
use std::time::Duration;

use crate::clock::Clock;
use crate::remote::{ChecksumAlgo, Listing, NodeId, NodeKind, RemoteEntry, SizeHint, Validator};
use crate::store::naming::display_names;

/// A node as the cache holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedNode {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub name: String,
    /// What the kernel is shown. Differs from `name` only when the raw name
    /// needed sanitising or disambiguating.
    pub display_name: String,
    pub kind: NodeKind,
    pub size: SizeHint,
    pub mtime: Option<i64>,
    pub mime: Option<String>,
    pub validator: Option<Validator>,
    /// Blob name in the content cache, if content is local.
    pub content_key: Option<String>,
    pub content_len: Option<u64>,
    pub pinned: bool,
    pub sync_state: SyncState,
}

impl CachedNode {
    pub fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }

    /// Whether local content is present.
    pub fn is_cached(&self) -> bool {
        self.content_key.is_some()
    }
}

/// Per-node sync state.
///
/// Only [`SyncState::Clean`] is reachable this round; the rest exist so the
/// `user.davfs.status` vocabulary from spec/05 is frozen from the start and the
/// Nautilus extension never needs a second update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum SyncState {
    Clean = 0,
    Dirty = 1,
    Uploading = 2,
    Conflict = 3,
    Error = 4,
}

impl SyncState {
    fn from_i64(v: i64) -> Self {
        match v {
            1 => SyncState::Dirty,
            2 => SyncState::Uploading,
            3 => SyncState::Conflict,
            4 => SyncState::Error,
            _ => SyncState::Clean,
        }
    }

    /// The `user.davfs.status` value for this state.
    pub fn as_status(self, cached: bool) -> &'static str {
        match self {
            SyncState::Clean if cached => "local",
            SyncState::Clean => "remote",
            SyncState::Dirty => "modified",
            SyncState::Uploading => "syncing",
            SyncState::Conflict => "conflict",
            SyncState::Error => "error",
        }
    }
}

/// A directory's cached children plus the freshness of that listing.
#[derive(Debug, Clone)]
pub struct CachedListing {
    pub children: Vec<CachedNode>,
    pub validator: Option<Validator>,
    /// How long ago the listing was written. `None` if never listed.
    pub age: Option<Duration>,
    pub complete: bool,
}

impl CachedListing {
    pub fn is_fresh(&self, ttl: Duration) -> bool {
        self.age.map(|a| a < ttl).unwrap_or(false)
    }
}

#[derive(Clone)]
pub struct MetaStore {
    pool: SqlitePool,
    ttl: Duration,
    clock: Clock,
}

impl MetaStore {
    pub async fn new(db_path: &str, ttl: Duration) -> Result<Self> {
        Self::with_clock(db_path, ttl, Clock::System).await
    }

    pub async fn with_clock(db_path: &str, ttl: Duration, clock: Clock) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(db_path)?
            .create_if_missing(true)
            // sqlx enables this by default; stated so nobody has to check.
            .foreign_keys(true)
            // Matters once FUSE operations stop being serialized.
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5));

        let pool = SqlitePool::connect_with(options).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;

        Ok(Self { pool, ttl, clock })
    }

    /// An in-process store for tests.
    #[cfg(test)]
    pub async fn in_temp_dir(ttl: Duration, clock: Clock) -> Result<(tempfile::TempDir, Self)> {
        let dir = tempfile::tempdir()?;
        let url = format!("sqlite://{}", dir.path().join("meta.db").display());
        let store = Self::with_clock(&url, ttl, clock).await?;
        Ok((dir, store))
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Seed the root node and record which account this database belongs to.
    ///
    /// If the stored backend or root disagrees with the caller's, the cache is
    /// wiped and re-seeded: that is the "the mount was repointed at a different
    /// server" case, which otherwise silently serves the wrong listings.
    pub async fn ensure_root(&self, root: &NodeId, backend_tag: &str) -> Result<()> {
        let stored_backend: Option<String> =
            sqlx::query_scalar("SELECT v FROM account WHERE k = 'backend'")
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        let stored_root: Option<String> =
            sqlx::query_scalar("SELECT v FROM account WHERE k = 'root_id'")
                .fetch_optional(&self.pool)
                .await?
                .flatten();

        let mismatch = match (&stored_backend, &stored_root) {
            (Some(b), Some(r)) => b != backend_tag || r != root.as_str(),
            _ => false,
        };
        if mismatch {
            tracing::warn!(
                "cache belongs to a different account ({:?}/{:?} != {}/{}); clearing",
                stored_backend,
                stored_root,
                backend_tag,
                root
            );
            self.clear().await?;
        }

        let now = self.clock.now_secs();
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO nodes (remote_id, parent_id, name, display_name, kind, fetched_at) \
             VALUES (?, NULL, '', '', ?, ?) ON CONFLICT(remote_id) DO NOTHING",
        )
        .bind(root.as_str())
        .bind(kind_to_i64(NodeKind::Dir))
        .bind(now)
        .execute(&mut *tx)
        .await?;

        // Explicit ino 1 so the root matches FUSE_ROOT_ID.
        sqlx::query(
            "INSERT INTO inodes (ino, remote_id) VALUES (1, ?) ON CONFLICT(ino) DO NOTHING",
        )
        .bind(root.as_str())
        .execute(&mut *tx)
        .await?;

        for (k, v) in [("backend", backend_tag), ("root_id", root.as_str())] {
            sqlx::query(
                "INSERT INTO account (k, v) VALUES (?, ?) \
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            )
            .bind(k)
            .bind(v)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    // -- reading ----------------------------------------------------------

    pub async fn node(&self, id: &NodeId) -> Option<CachedNode> {
        let row = sqlx::query(NODE_COLUMNS_SQL)
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .ok()??;
        Some(row_to_node(&row))
    }

    /// A directory's children, with the freshness of the listing.
    ///
    /// Returns `None` only when the directory itself is unknown. A directory
    /// that exists but has never been listed yields `age: None`, which is
    /// distinct from "listed and found empty".
    pub async fn children(&self, dir: &NodeId) -> Option<CachedListing> {
        let dir_row = sqlx::query(
            "SELECT children_validator, children_validator_kind, listed_at, listing_complete \
             FROM nodes WHERE remote_id = ?",
        )
        .bind(dir.as_str())
        .fetch_optional(&self.pool)
        .await
        .ok()??;

        let validator = validator_from(dir_row.get(1), dir_row.get(0));
        let listed_at: Option<i64> = dir_row.get(2);
        let complete: i64 = dir_row.get(3);

        let rows = sqlx::query(&format!(
            "{NODE_COLUMNS_BASE} WHERE parent_id = ? ORDER BY display_name"
        ))
        .bind(dir.as_str())
        .fetch_all(&self.pool)
        .await
        .ok()?;

        Some(CachedListing {
            children: rows.iter().map(row_to_node).collect(),
            validator,
            age: listed_at.map(|t| {
                Duration::from_secs(self.clock.now_secs().saturating_sub(t).max(0) as u64)
            }),
            complete: complete != 0,
        })
    }

    /// Resolve one child by the name the kernel uses.
    pub async fn lookup_child(&self, parent: &NodeId, display_name: &str) -> Option<CachedNode> {
        let row = sqlx::query(&format!(
            "{NODE_COLUMNS_BASE} WHERE parent_id = ? AND display_name = ?"
        ))
        .bind(parent.as_str())
        .bind(display_name)
        .fetch_optional(&self.pool)
        .await
        .ok()??;
        Some(row_to_node(&row))
    }

    // -- writing ----------------------------------------------------------

    /// Write a listing, computing display names over the whole sibling set.
    ///
    /// One transaction. Children absent from the listing are deleted only when
    /// the listing is `complete`; otherwise a paginated backend would drop
    /// everything past its first page.
    pub async fn upsert_listing(&self, dir: &NodeId, listing: &Listing) -> Result<()> {
        let Listing::Fresh {
            validator,
            children,
            complete,
            ..
        } = listing
        else {
            // Unchanged: only the freshness timestamp moves.
            return self.touch_listing(dir).await;
        };

        let now = self.clock.now_secs();
        let names = display_names(children);
        let mut tx = self.pool.begin().await?;

        for (entry, display) in children.iter().zip(&names) {
            upsert_node(&mut tx, entry, display, Some(dir), now).await?;
        }

        if *complete {
            // Delete children that vanished. A NOT IN over the kept ids.
            let keep: Vec<String> = children.iter().map(|c| c.id.0.clone()).collect();
            let placeholders = std::iter::repeat_n("?", keep.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = if keep.is_empty() {
                "DELETE FROM nodes WHERE parent_id = ?".to_string()
            } else {
                format!(
                    "DELETE FROM nodes WHERE parent_id = ? AND remote_id NOT IN ({placeholders})"
                )
            };
            let mut q = sqlx::query(&sql).bind(dir.as_str());
            for k in &keep {
                q = q.bind(k);
            }
            q.execute(&mut *tx).await?;
        }

        sqlx::query(
            "UPDATE nodes SET children_validator = ?, children_validator_kind = ?, \
             listed_at = ?, listing_complete = ? WHERE remote_id = ?",
        )
        .bind(validator.as_ref().map(|v| v.value()))
        .bind(validator.as_ref().map(|v| v.kind() as i64))
        .bind(now)
        .bind(i64::from(*complete))
        .bind(dir.as_str())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Mark a listing as revalidated without changing its contents.
    pub async fn touch_listing(&self, dir: &NodeId) -> Result<()> {
        sqlx::query("UPDATE nodes SET listed_at = ? WHERE remote_id = ?")
            .bind(self.clock.now_secs())
            .bind(dir.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Insert or update a single node, e.g. from a `stat` or a delta.
    pub async fn upsert_node(&self, entry: &RemoteEntry, display_name: &str) -> Result<()> {
        let now = self.clock.now_secs();
        let mut tx = self.pool.begin().await?;
        let parent = entry.parent.clone();
        upsert_node(&mut tx, entry, display_name, parent.as_ref(), now).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Record that a node's content is now in the blob cache.
    pub async fn set_content(
        &self,
        id: &NodeId,
        key: &str,
        len: u64,
        validator: Option<&Validator>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE nodes SET content_key = ?, content_len = ?, content_validator = ?, \
             accessed_at = ?, size = COALESCE(size, ?) WHERE remote_id = ?",
        )
        .bind(key)
        .bind(len as i64)
        .bind(validator.map(|v| v.value()))
        .bind(self.clock.now_secs())
        .bind(len as i64)
        .bind(id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Forget a node's local content. Always called *before* the blob is
    /// unlinked, so the database never points at a file that is not there.
    pub async fn clear_content(&self, id: &NodeId) -> Result<Option<String>> {
        let key: Option<String> =
            sqlx::query_scalar("SELECT content_key FROM nodes WHERE remote_id = ?")
                .bind(id.as_str())
                .fetch_optional(&self.pool)
                .await?
                .flatten();

        sqlx::query(
            "UPDATE nodes SET content_key = NULL, content_len = NULL, content_validator = NULL \
             WHERE remote_id = ?",
        )
        .bind(id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(key)
    }

    /// Update the LRU timestamp. Called at `open`, not per `read`.
    pub async fn touch_access(&self, id: &NodeId) -> Result<()> {
        sqlx::query("UPDATE nodes SET accessed_at = ? WHERE remote_id = ?")
            .bind(self.clock.now_secs())
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_pinned(&self, id: &NodeId, pinned: bool) -> Result<()> {
        sqlx::query("UPDATE nodes SET pinned = ? WHERE remote_id = ?")
            .bind(i64::from(pinned))
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Drop a node and, by cascade, its whole subtree.
    pub async fn remove(&self, id: &NodeId) -> Result<()> {
        sqlx::query("DELETE FROM nodes WHERE remote_id = ?")
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Invalidate a directory's listing without deleting its children, so the
    /// next access revalidates but stale data can still be served meanwhile.
    pub async fn invalidate_listing(&self, dir: &NodeId) -> Result<()> {
        sqlx::query("UPDATE nodes SET listed_at = NULL WHERE remote_id = ?")
            .bind(dir.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn clear(&self) -> Result<()> {
        // inodes deliberately survive: an inode must outlive a metadata purge.
        sqlx::query("DELETE FROM nodes").execute(&self.pool).await?;
        Ok(())
    }

    // -- inodes -----------------------------------------------------------

    /// The inode for a node, allocating one if needed.
    ///
    /// Persisted, so inode numbers are stable across remounts -- unlike the
    /// PoC's in-memory counter, which renumbered everything on every mount.
    pub async fn ino_for(&self, id: &NodeId) -> Result<u64> {
        if let Some(ino) =
            sqlx::query_scalar::<_, i64>("SELECT ino FROM inodes WHERE remote_id = ?")
                .bind(id.as_str())
                .fetch_optional(&self.pool)
                .await?
        {
            return Ok(ino as u64);
        }

        sqlx::query("INSERT INTO inodes (remote_id) VALUES (?) ON CONFLICT(remote_id) DO NOTHING")
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;

        let ino: i64 = sqlx::query_scalar("SELECT ino FROM inodes WHERE remote_id = ?")
            .bind(id.as_str())
            .fetch_one(&self.pool)
            .await?;
        Ok(ino as u64)
    }

    pub async fn node_for_ino(&self, ino: u64) -> Option<NodeId> {
        sqlx::query_scalar::<_, String>("SELECT remote_id FROM inodes WHERE ino = ?")
            .bind(ino as i64)
            .fetch_optional(&self.pool)
            .await
            .ok()?
            .map(NodeId)
    }

    // -- account state ----------------------------------------------------

    pub async fn get_account(&self, key: &str) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>("SELECT v FROM account WHERE k = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .ok()?
            .flatten()
    }

    pub async fn set_account(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO account (k, v) VALUES (?, ?) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // -- maintenance ------------------------------------------------------

    /// Directories whose listing is older than `max_age`, oldest first.
    pub async fn stale_directories(&self, max_age: Duration, limit: i64) -> Result<Vec<NodeId>> {
        let cutoff = self.clock.now_secs() - max_age.as_secs() as i64;
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT remote_id FROM nodes WHERE kind = ? AND (listed_at IS NULL OR listed_at < ?) \
             ORDER BY COALESCE(listed_at, 0) ASC LIMIT ?",
        )
        .bind(kind_to_i64(NodeKind::Dir))
        .bind(cutoff)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(NodeId).collect())
    }

    /// Blob keys eligible for eviction, least recently used first.
    ///
    /// Pinned nodes and anything with unsynced local changes are excluded.
    pub async fn eviction_candidates(&self, limit: i64) -> Result<Vec<(NodeId, String, u64)>> {
        let rows = sqlx::query(
            "SELECT remote_id, content_key, COALESCE(content_len, 0) FROM nodes \
             WHERE content_key IS NOT NULL AND pinned = 0 AND sync_state = 0 \
             ORDER BY COALESCE(accessed_at, 0) ASC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| (NodeId(r.get(0)), r.get(1), r.get::<i64, _>(2) as u64))
            .collect())
    }

    /// Every blob key the database currently points at.
    ///
    /// Used to find orphans on disk, which can only come from a crash between
    /// the blob rename and the row update.
    pub async fn all_content_keys(&self) -> Result<Vec<String>> {
        Ok(
            sqlx::query_scalar("SELECT content_key FROM nodes WHERE content_key IS NOT NULL")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Forget directory listings not touched within `keep`.
    ///
    /// Only the listing is dropped, not the nodes: the next access revalidates
    /// but inodes and any cached content survive. Startup-only, because pruning
    /// while FUSE traffic flows races the kernel's dentry cache.
    pub async fn prune_unlisted(&self, keep: Duration) -> Result<usize> {
        let cutoff = self.clock.now_secs() - keep.as_secs() as i64;
        let n = sqlx::query(
            "UPDATE nodes SET listed_at = NULL WHERE kind = ? AND listed_at IS NOT NULL \
             AND listed_at < ?",
        )
        .bind(kind_to_i64(NodeKind::Dir))
        .bind(cutoff)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n as usize)
    }

    pub async fn total_content_bytes(&self) -> Result<u64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(content_len), 0) FROM nodes WHERE content_key IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(n as u64)
    }

    pub async fn stats(&self) -> Result<CacheStats> {
        let cutoff = self.clock.now_secs() - self.ttl.as_secs() as i64;
        let nodes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nodes")
            .fetch_one(&self.pool)
            .await?;
        let listed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM nodes WHERE kind = ? AND listed_at IS NOT NULL",
        )
        .bind(kind_to_i64(NodeKind::Dir))
        .fetch_one(&self.pool)
        .await?;
        let fresh: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE kind = ? AND listed_at >= ?")
                .bind(kind_to_i64(NodeKind::Dir))
                .bind(cutoff)
                .fetch_one(&self.pool)
                .await?;
        let cached: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE content_key IS NOT NULL")
                .fetch_one(&self.pool)
                .await?;

        Ok(CacheStats {
            nodes: nodes as usize,
            listed_directories: listed as usize,
            fresh_directories: fresh as usize,
            cached_files: cached as usize,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheStats {
    pub nodes: usize,
    pub listed_directories: usize,
    pub fresh_directories: usize,
    pub cached_files: usize,
}

// ---------------------------------------------------------------------------
// Row mapping
// ---------------------------------------------------------------------------

const NODE_COLUMNS_BASE: &str = "SELECT remote_id, parent_id, name, display_name, kind, size, \
     mtime, mime, validator_kind, validator, content_key, content_len, pinned, sync_state \
     FROM nodes";

const NODE_COLUMNS_SQL: &str = "SELECT remote_id, parent_id, name, display_name, kind, size, \
     mtime, mime, validator_kind, validator, content_key, content_len, pinned, sync_state \
     FROM nodes WHERE remote_id = ?";

fn row_to_node(row: &sqlx::sqlite::SqliteRow) -> CachedNode {
    let size: Option<i64> = row.get(5);
    CachedNode {
        id: NodeId(row.get(0)),
        parent: row.get::<Option<String>, _>(1).map(NodeId),
        name: row.get(2),
        display_name: row.get(3),
        kind: kind_from_i64(row.get(4)),
        // NULL means Unknown, which is why the column is nullable: 0 would
        // claim a Google-native doc is an empty file.
        size: match size {
            Some(n) => SizeHint::Exact(n as u64),
            None => SizeHint::Unknown,
        },
        mtime: row.get(6),
        mime: row.get(7),
        validator: validator_from(row.get(8), row.get(9)),
        content_key: row.get(10),
        content_len: row.get::<Option<i64>, _>(11).map(|n| n as u64),
        pinned: row.get::<i64, _>(12) != 0,
        sync_state: SyncState::from_i64(row.get(13)),
    }
}

async fn upsert_node(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    entry: &RemoteEntry,
    display_name: &str,
    parent: Option<&NodeId>,
    now: i64,
) -> Result<()> {
    let (vkind, vval) = match &entry.validator {
        Some(v) => (Some(v.kind() as i64), Some(v.value())),
        None => (None, None),
    };
    let (calgo, chex) = match &entry.validator {
        Some(Validator::Checksum { algo, hex }) => (Some(algo_str(*algo)), Some(hex.clone())),
        _ => (None, None),
    };

    // ON CONFLICT DO UPDATE, never INSERT OR REPLACE: REPLACE deletes the row,
    // which fires ON DELETE CASCADE and takes the node's whole subtree with it.
    sqlx::query(
        "INSERT INTO nodes (remote_id, parent_id, name, display_name, kind, size, mtime, mime, \
                            validator_kind, validator, checksum_algo, checksum, target_id, fetched_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(remote_id) DO UPDATE SET \
            parent_id = excluded.parent_id, name = excluded.name, \
            display_name = excluded.display_name, kind = excluded.kind, size = excluded.size, \
            mtime = excluded.mtime, mime = excluded.mime, \
            validator_kind = excluded.validator_kind, validator = excluded.validator, \
            checksum_algo = excluded.checksum_algo, checksum = excluded.checksum, \
            target_id = excluded.target_id, fetched_at = excluded.fetched_at",
    )
    .bind(entry.id.as_str())
    .bind(parent.map(|p| p.0.clone()))
    .bind(&entry.name)
    .bind(display_name)
    .bind(kind_to_i64(entry.kind))
    .bind(entry.size.exact().map(|n| n as i64))
    .bind(entry.mtime.map(|m| m.timestamp()))
    .bind(entry.mime.clone())
    .bind(vkind)
    .bind(vval)
    .bind(calgo)
    .bind(chex)
    .bind(entry.target.as_ref().map(|t| t.0.clone()))
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn kind_to_i64(k: NodeKind) -> i64 {
    match k {
        NodeKind::File => 0,
        NodeKind::Dir => 1,
        NodeKind::Symlink => 2,
        NodeKind::ExportOnly => 3,
        NodeKind::Other => 4,
    }
}

fn kind_from_i64(v: i64) -> NodeKind {
    match v {
        1 => NodeKind::Dir,
        2 => NodeKind::Symlink,
        3 => NodeKind::ExportOnly,
        4 => NodeKind::Other,
        _ => NodeKind::File,
    }
}

fn algo_str(a: ChecksumAlgo) -> &'static str {
    match a {
        ChecksumAlgo::Md5 => "md5",
        ChecksumAlgo::Sha1 => "sha1",
        ChecksumAlgo::Sha256 => "sha256",
    }
}

fn validator_from(kind: Option<i64>, value: Option<String>) -> Option<Validator> {
    let (kind, value) = (kind?, value?);
    Some(match kind {
        0 => Validator::Etag(value),
        1 => Validator::Version(value.parse().ok()?),
        2 => Validator::Checksum {
            algo: ChecksumAlgo::Md5,
            hex: value,
        },
        _ => {
            let (size, mtime) = value.split_once(':')?;
            Validator::SizeMtime {
                size: size.parse().ok()?,
                mtime_unix: mtime.parse().ok()?,
            }
        }
    })
}

/// Display name for an entry with no sibling context (a `stat` result).
pub fn single_display_name(entry: &RemoteEntry) -> String {
    display_names(std::slice::from_ref(entry))
        .into_iter()
        .next()
        .unwrap_or_default()
}
