//! SQLite metadata cache.
//!
//! Phase 5 replaces the `directories`/`entries` pair with a backend-neutral
//! id-keyed `nodes` table. This is the same schema as before, restructured so
//! it is testable: the clock is injected and the three near-identical readers
//! share one row-mapping path.

use anyhow::Result;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous},
    Row, SqlitePool,
};
use std::str::FromStr;
use std::time::Duration;

use crate::clock::Clock;
use crate::webdav::DavEntry;

/// A cached directory listing plus the metadata needed to decide what to do
/// with it.
pub struct CachedListing {
    pub entries: Vec<DavEntry>,
    /// Validator for conditional revalidation.
    pub etag: Option<String>,
    /// How long ago this listing was written.
    pub age: Duration,
}

impl CachedListing {
    pub fn is_fresh(&self, ttl: Duration) -> bool {
        self.age < ttl
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
            // sqlx already enables foreign_keys by default; stated explicitly
            // so nobody has to go and check.
            .foreign_keys(true)
            // WAL matters once FUSE operations stop being serialized.
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5));

        let pool = SqlitePool::connect_with(options).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;

        Ok(Self { pool, ttl, clock })
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Close the pool. Called on shutdown so SQLite is not left mid-checkpoint.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// The one place a directory row and its entries are read.
    ///
    /// Returns regardless of age; callers decide using [`CachedListing::is_fresh`].
    pub async fn load(&self, path: &str) -> Option<CachedListing> {
        let dir_row = sqlx::query("SELECT etag, cached_at FROM directories WHERE path = ?")
            .bind(path)
            .fetch_optional(&self.pool)
            .await
            .ok()??;

        let etag: Option<String> = dir_row.get(0);
        let cached_at: i64 = dir_row.get(1);
        // saturating: a clock that went backwards must not panic on subtraction.
        let age =
            Duration::from_secs(self.clock.now_secs().saturating_sub(cached_at).max(0) as u64);

        let rows = sqlx::query(
            "SELECT name, is_dir, size, modified FROM entries \
             WHERE directory_path = ? ORDER BY name",
        )
        .bind(path)
        .fetch_all(&self.pool)
        .await
        .ok()?;

        let entries = rows
            .iter()
            .map(|row| {
                let modified_secs: Option<i64> = row.get(3);
                DavEntry {
                    name: row.get(0),
                    is_dir: row.get(1),
                    size: row.get::<i64, _>(2) as u64,
                    // from_timestamp returns None for out-of-range values.
                    // The PoC unwrapped this, so a hostile or corrupt stored
                    // value panicked the whole filesystem.
                    modified: modified_secs.and_then(|s| chrono::DateTime::from_timestamp(s, 0)),
                }
            })
            .collect();

        Some(CachedListing { entries, etag, age })
    }

    /// Cached entries, only if within the TTL.
    pub async fn get(&self, path: &str) -> Option<Vec<DavEntry>> {
        let listing = self.load(path).await?;
        if listing.is_fresh(self.ttl) {
            tracing::debug!("Cache hit for {}", path);
            Some(listing.entries)
        } else {
            tracing::debug!("Cache expired for {} (age {:?})", path, listing.age);
            None
        }
    }

    /// Cached entries regardless of age. This is what the FUSE layer serves
    /// from, so a slow or unreachable server never blocks a listing.
    pub async fn get_stale(&self, path: &str) -> Option<Vec<DavEntry>> {
        Some(self.load(path).await?.entries)
    }

    /// Cached entries plus the validator for a conditional revalidation.
    pub async fn get_with_etag(&self, path: &str) -> Option<(Vec<DavEntry>, Option<String>)> {
        let listing = self.load(path).await?;
        Some((listing.entries, listing.etag))
    }

    pub async fn insert(
        &self,
        path: String,
        entries: Vec<DavEntry>,
        etag: Option<String>,
    ) -> Result<()> {
        let now = self.clock.now_secs();

        let mut tx = self.pool.begin().await?;

        // ON CONFLICT DO UPDATE, never INSERT OR REPLACE: REPLACE deletes the
        // parent row, which fires ON DELETE CASCADE and wipes every child in
        // `entries`. The old code survived that only because it re-inserted the
        // children three statements later.
        sqlx::query(
            "INSERT INTO directories (path, etag, cached_at) VALUES (?, ?, ?) \
             ON CONFLICT(path) DO UPDATE SET etag = excluded.etag, cached_at = excluded.cached_at",
        )
        .bind(&path)
        .bind(&etag)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM entries WHERE directory_path = ?")
            .bind(&path)
            .execute(&mut *tx)
            .await?;

        for entry in &entries {
            sqlx::query(
                "INSERT INTO entries (directory_path, name, is_dir, size, modified) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(&path)
            .bind(&entry.name)
            .bind(entry.is_dir)
            .bind(entry.size as i64)
            .bind(entry.modified.map(|dt| dt.timestamp()))
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        tracing::debug!(
            "Cached {} entries for {} (etag {:?})",
            entries.len(),
            path,
            etag
        );
        Ok(())
    }

    pub async fn invalidate(&self, path: &str) -> Result<()> {
        sqlx::query("DELETE FROM directories WHERE path = ?")
            .bind(path)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn clear(&self) -> Result<()> {
        sqlx::query("DELETE FROM directories")
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM entries")
            .execute(&self.pool)
            .await?;
        tracing::info!("Cleared all cache entries");
        Ok(())
    }

    pub async fn stats(&self) -> Result<CacheStats> {
        let cutoff = self.clock.now_secs() - self.ttl.as_secs() as i64;

        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM directories")
            .fetch_one(&self.pool)
            .await?;
        let expired: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM directories WHERE cached_at < ?")
                .bind(cutoff)
                .fetch_one(&self.pool)
                .await?;

        Ok(CacheStats {
            total_directories: total as usize,
            expired_directories: expired as usize,
            active_directories: (total - expired) as usize,
        })
    }
}

pub struct CacheStats {
    pub total_directories: usize,
    pub expired_directories: usize,
    pub active_directories: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A store backed by a real file.
    ///
    /// Deliberately not `sqlite::memory:`: every pooled connection would get
    /// its own private empty database, so migrations run on connection #1 and
    /// the query lands on #2 with "no such table". A tempfile is ~1ms and also
    /// exercises the real create_if_missing path.
    ///
    /// The TempDir is returned because it must outlive the store.
    async fn store(ttl: Duration, clock: Clock) -> (TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("meta.db").display());
        let s = MetaStore::with_clock(&url, ttl, clock).await.unwrap();
        (dir, s)
    }

    fn entry(name: &str, is_dir: bool, size: u64) -> DavEntry {
        DavEntry {
            name: name.to_string(),
            is_dir,
            size,
            modified: chrono::DateTime::from_timestamp(1_600_000_000, 0),
        }
    }

    #[tokio::test]
    async fn migrations_apply_to_a_fresh_database() {
        let (_d, s) = store(Duration::from_secs(5), Clock::System).await;
        assert_eq!(s.stats().await.unwrap().total_directories, 0);
    }

    #[tokio::test]
    async fn insert_then_get_round_trips() {
        let (_d, s) = store(Duration::from_secs(30), Clock::fixed(1000)).await;
        s.insert(
            "/".into(),
            vec![entry("a.txt", false, 12), entry("sub", true, 0)],
            Some("\"etag1\"".into()),
        )
        .await
        .unwrap();

        let got = s.get("/").await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "a.txt");
        assert!(!got[0].is_dir);
        assert_eq!(got[0].size, 12);
        assert!(got[1].is_dir);

        let (_, etag) = s.get_with_etag("/").await.unwrap();
        assert_eq!(etag.as_deref(), Some("\"etag1\""));
    }

    #[tokio::test]
    async fn ttl_boundary_is_exact() {
        let clock = Clock::fixed(1000);
        let (_d, s) = store(Duration::from_secs(30), clock.clone()).await;
        s.insert("/".into(), vec![entry("a", false, 1)], None)
            .await
            .unwrap();

        clock.advance(29);
        assert!(s.get("/").await.is_some(), "29s < 30s TTL should be fresh");

        clock.advance(1);
        assert!(s.get("/").await.is_none(), "30s == TTL should be expired");

        // ...but stale reads still serve, which is what keeps the mount usable.
        assert_eq!(s.get_stale("/").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reinsert_replaces_rather_than_duplicates() {
        let (_d, s) = store(Duration::from_secs(30), Clock::fixed(1000)).await;
        s.insert("/".into(), vec![entry("a", false, 1)], None)
            .await
            .unwrap();
        s.insert(
            "/".into(),
            vec![entry("b", false, 2), entry("c", false, 3)],
            None,
        )
        .await
        .unwrap();

        let got = s.get("/").await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "b");
    }

    #[tokio::test]
    async fn invalidate_cascades_to_entries() {
        // Passes only because foreign_keys is ON; this pins that.
        let (_d, s) = store(Duration::from_secs(30), Clock::fixed(1000)).await;
        s.insert("/d".into(), vec![entry("a", false, 1)], None)
            .await
            .unwrap();
        s.invalidate("/d").await.unwrap();

        assert!(s.get_stale("/d").await.is_none());
        let orphans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entries")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(orphans, 0, "ON DELETE CASCADE did not fire");
    }

    #[tokio::test]
    async fn sizes_above_i32_max_survive() {
        let (_d, s) = store(Duration::from_secs(30), Clock::fixed(1000)).await;
        let big = 8_589_934_592; // 8 GiB
        s.insert("/".into(), vec![entry("big.iso", false, big)], None)
            .await
            .unwrap();
        assert_eq!(s.get("/").await.unwrap()[0].size, big);
    }

    #[tokio::test]
    async fn absent_and_out_of_range_timestamps_do_not_panic() {
        let (_d, s) = store(Duration::from_secs(30), Clock::fixed(1000)).await;
        let mut none_mtime = entry("a", false, 1);
        none_mtime.modified = None;
        s.insert("/".into(), vec![none_mtime], None).await.unwrap();
        assert!(s.get("/").await.unwrap()[0].modified.is_none());

        // A corrupt row must degrade to None, not panic the filesystem.
        sqlx::query("UPDATE entries SET modified = ? WHERE name = 'a'")
            .bind(i64::MIN)
            .execute(&s.pool)
            .await
            .unwrap();
        assert!(s.get("/").await.unwrap()[0].modified.is_none());
    }

    #[tokio::test]
    async fn names_with_unicode_and_percent_round_trip() {
        let (_d, s) = store(Duration::from_secs(30), Clock::fixed(1000)).await;
        let names = ["ümlaut Ä.txt", "100% done.txt", "emoji 🚀.md", "a#b?c.txt"];
        s.insert(
            "/".into(),
            names.iter().map(|n| entry(n, false, 1)).collect(),
            None,
        )
        .await
        .unwrap();

        let mut got: Vec<String> = s
            .get("/")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        got.sort();
        let mut want: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(got, want);
    }

    #[tokio::test]
    async fn stats_split_fresh_from_expired() {
        let clock = Clock::fixed(1000);
        let (_d, s) = store(Duration::from_secs(30), clock.clone()).await;
        s.insert("/old".into(), vec![], None).await.unwrap();
        clock.advance(60);
        s.insert("/new".into(), vec![], None).await.unwrap();

        let st = s.stats().await.unwrap();
        assert_eq!(st.total_directories, 2);
        assert_eq!(st.expired_directories, 1);
        assert_eq!(st.active_directories, 1);
    }

    #[tokio::test]
    async fn concurrent_inserts_do_not_deadlock() {
        let (_d, s) = store(Duration::from_secs(30), Clock::fixed(1000)).await;
        let a = s.clone();
        let b = s.clone();
        let (ra, rb) = tokio::join!(
            tokio::spawn(async move {
                a.insert("/a".into(), vec![entry("x", false, 1)], None)
                    .await
            }),
            tokio::spawn(async move {
                b.insert("/b".into(), vec![entry("y", false, 1)], None)
                    .await
            }),
        );
        ra.unwrap().unwrap();
        rb.unwrap().unwrap();
        assert_eq!(s.stats().await.unwrap().total_directories, 2);
    }
}
