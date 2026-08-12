use anyhow::Result;
use sqlx::{sqlite::SqliteConnectOptions, Row, SqlitePool};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::webdav::DavEntry;

#[derive(Clone)]
pub struct DirectoryCache {
    pool: SqlitePool,
    ttl: Duration,
}

impl DirectoryCache {
    pub async fn new(db_path: &str, ttl: Duration) -> Result<Self> {
        // Create connection options that will create the database if it doesn't exist
        let options = SqliteConnectOptions::from_str(db_path)?.create_if_missing(true);

        let pool = SqlitePool::connect_with(options).await?;

        // Run migrations
        sqlx::migrate!("./migrations").run(&pool).await?;

        Ok(Self { pool, ttl })
    }

    pub async fn get(&self, path: &str) -> Option<Vec<DavEntry>> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Check if directory exists and is fresh
        let dir_row = sqlx::query("SELECT cached_at FROM directories WHERE path = ?")
            .bind(path)
            .fetch_optional(&self.pool)
            .await
            .ok()??;

        let cached_at: i64 = dir_row.get(0);
        let age = Duration::from_secs((now - cached_at) as u64);

        if age < self.ttl {
            // Fetch entries
            let rows = sqlx::query(
                "SELECT name, is_dir, size, modified FROM entries WHERE directory_path = ? ORDER BY name"
            )
            .bind(path)
            .fetch_all(&self.pool)
            .await
            .ok()?;

            let entries: Vec<DavEntry> = rows
                .iter()
                .map(|row| {
                    let modified_secs: Option<i64> = row.get(3);
                    DavEntry {
                        name: row.get(0),
                        is_dir: row.get(1),
                        size: row.get::<i64, _>(2) as u64,
                        modified: modified_secs.map(|secs| {
                            chrono::DateTime::from_timestamp(secs, 0)
                                .unwrap()
                                .with_timezone(&chrono::Utc)
                        }),
                    }
                })
                .collect();

            tracing::debug!("Cache hit for path: {}", path);
            return Some(entries);
        } else {
            tracing::debug!("Cache expired for path: {}", path);
        }

        tracing::debug!("Cache miss for path: {}", path);
        None
    }

    pub async fn get_stale(&self, path: &str) -> Option<Vec<DavEntry>> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Check if directory exists
        let dir_row = sqlx::query("SELECT cached_at FROM directories WHERE path = ?")
            .bind(path)
            .fetch_optional(&self.pool)
            .await
            .ok()??;

        let cached_at: i64 = dir_row.get(0);
        let age = Duration::from_secs((now - cached_at) as u64);

        // Fetch entries
        let rows = sqlx::query(
            "SELECT name, is_dir, size, modified FROM entries WHERE directory_path = ? ORDER BY name"
        )
        .bind(path)
        .fetch_all(&self.pool)
        .await
        .ok()?;

        let entries: Vec<DavEntry> = rows
            .iter()
            .map(|row| {
                let modified_secs: Option<i64> = row.get(3);
                DavEntry {
                    name: row.get(0),
                    is_dir: row.get(1),
                    size: row.get::<i64, _>(2) as u64,
                    modified: modified_secs.map(|secs| {
                        chrono::DateTime::from_timestamp(secs, 0)
                            .unwrap()
                            .with_timezone(&chrono::Utc)
                    }),
                }
            })
            .collect();

        if age < self.ttl {
            tracing::debug!("Cache hit (fresh) for path: {}", path);
        } else {
            tracing::debug!("Cache hit (stale, age={:?}) for path: {}", age, path);
        }
        Some(entries)
    }

    pub async fn get_with_etag(&self, path: &str) -> Option<(Vec<DavEntry>, Option<String>)> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Check if directory exists
        let dir_row = sqlx::query("SELECT etag, cached_at FROM directories WHERE path = ?")
            .bind(path)
            .fetch_optional(&self.pool)
            .await
            .ok()??;

        let etag: Option<String> = dir_row.get(0);
        let cached_at: i64 = dir_row.get(1);
        let age = Duration::from_secs((now - cached_at) as u64);

        // Fetch entries
        let rows = sqlx::query(
            "SELECT name, is_dir, size, modified FROM entries WHERE directory_path = ? ORDER BY name"
        )
        .bind(path)
        .fetch_all(&self.pool)
        .await
        .ok()?;

        let entries: Vec<DavEntry> = rows
            .iter()
            .map(|row| {
                let modified_secs: Option<i64> = row.get(3);
                DavEntry {
                    name: row.get(0),
                    is_dir: row.get(1),
                    size: row.get::<i64, _>(2) as u64,
                    modified: modified_secs.map(|secs| {
                        chrono::DateTime::from_timestamp(secs, 0)
                            .unwrap()
                            .with_timezone(&chrono::Utc)
                    }),
                }
            })
            .collect();

        if age < self.ttl {
            tracing::debug!("Cache hit (fresh) for path: {}", path);
        } else {
            tracing::debug!(
                "Cache hit (stale, age={:?}) for path: {} with etag: {:?}",
                age,
                path,
                etag
            );
        }
        Some((entries, etag))
    }

    pub async fn insert(
        &self,
        path: String,
        entries: Vec<DavEntry>,
        etag: Option<String>,
    ) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Start a transaction
        let mut tx = self.pool.begin().await?;

        // Insert or update directory
        sqlx::query("INSERT OR REPLACE INTO directories (path, etag, cached_at) VALUES (?, ?, ?)")
            .bind(&path)
            .bind(&etag)
            .bind(now)
            .execute(&mut *tx)
            .await?;

        // Delete old entries for this directory
        sqlx::query("DELETE FROM entries WHERE directory_path = ?")
            .bind(&path)
            .execute(&mut *tx)
            .await?;

        // Insert new entries
        for entry in &entries {
            let modified_secs = entry.modified.map(|dt| dt.timestamp());
            sqlx::query(
                "INSERT INTO entries (directory_path, name, is_dir, size, modified) VALUES (?, ?, ?, ?, ?)"
            )
            .bind(&path)
            .bind(&entry.name)
            .bind(entry.is_dir)
            .bind(entry.size as i64)
            .bind(modified_secs)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        tracing::debug!(
            "Cached {} entries for path: {} with etag: {:?}",
            entries.len(),
            path,
            etag
        );
        Ok(())
    }

    pub async fn invalidate(&self, path: &str) -> Result<()> {
        // Cascade delete will remove entries automatically
        sqlx::query("DELETE FROM directories WHERE path = ?")
            .bind(path)
            .execute(&self.pool)
            .await?;

        tracing::debug!("Invalidated cache for path: {}", path);
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
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let ttl_secs = self.ttl.as_secs() as i64;

        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM directories")
            .fetch_one(&self.pool)
            .await?;

        let expired: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM directories WHERE cached_at < ?")
                .bind(now - ttl_secs)
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
