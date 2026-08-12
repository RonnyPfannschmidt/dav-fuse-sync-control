//! The background worker: revalidation, cache eviction, and online/offline
//! state.
//!
//! Everything here is periodic and best-effort. Nothing a FUSE handler needs is
//! allowed to depend on it having run: a mount that never starts the worker is
//! slower and grows without bound, but is never wrong.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::error::RemoteError;
use crate::vfs::Vfs;

#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// How often to look for directories worth revalidating.
    pub revalidate_interval: Duration,
    /// A listing older than this is a candidate.
    pub metadata_max_age: Duration,
    /// Directories revalidated per pass, so a large tree is spread out rather
    /// than hammered.
    pub revalidate_batch: i64,

    /// How often to run cache maintenance.
    pub gc_interval: Duration,
    /// Total content-cache budget.
    pub cache_max_bytes: u64,
    /// Keep at least this much free on the filesystem holding the cache.
    pub cache_min_free_bytes: u64,

    /// Backoff bounds while offline.
    pub offline_retry_min: Duration,
    pub offline_retry_max: Duration,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            revalidate_interval: Duration::from_secs(60),
            metadata_max_age: Duration::from_secs(15 * 60),
            revalidate_batch: 32,
            gc_interval: Duration::from_secs(5 * 60),
            cache_max_bytes: 10 * 1024 * 1024 * 1024,
            cache_min_free_bytes: 5 * 1024 * 1024 * 1024,
            offline_retry_min: Duration::from_secs(5),
            offline_retry_max: Duration::from_secs(5 * 60),
        }
    }
}

/// Whether the backend is currently reachable.
///
/// Shared so the daemon can report it (`sd_notify` STATUS) and the worker can
/// stop scheduling work that will only fail.
#[derive(Debug, Default)]
pub struct Health {
    online: AtomicBool,
    /// Consecutive failed probes, for backoff.
    failures: AtomicU64,
}

impl Health {
    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }

    pub fn record_success(&self) {
        self.online.store(true, Ordering::SeqCst);
        self.failures.store(0, Ordering::SeqCst);
    }

    pub fn record_failure(&self) {
        self.online.store(false, Ordering::SeqCst);
        self.failures.fetch_add(1, Ordering::SeqCst);
    }

    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::SeqCst)
    }

    /// A one-line description for `sd_notify` STATUS.
    pub fn status_line(&self) -> &'static str {
        if self.is_online() {
            "online"
        } else {
            "offline (serving from cache)"
        }
    }
}

pub struct SyncWorker {
    vfs: Arc<Vfs>,
    config: SyncConfig,
    health: Arc<Health>,
}

impl SyncWorker {
    pub fn new(vfs: Arc<Vfs>, config: SyncConfig, health: Arc<Health>) -> Self {
        Self {
            vfs,
            config,
            health,
        }
    }

    /// Run maintenance once. Exposed so tests do not have to wait for a timer.
    pub async fn run_once(&self) -> Maintenance {
        let revalidated = self.revalidate_pass().await;
        let evicted = self.gc_pass().await.unwrap_or_default();
        Maintenance {
            revalidated,
            evicted,
        }
    }

    /// Revalidate the stalest directories.
    ///
    /// Oldest first, so a tree that is never browsed still converges, and
    /// bounded per pass so a large mount does not produce a request storm.
    pub async fn revalidate_pass(&self) -> usize {
        if !self.health.is_online() {
            return 0;
        }

        let stale = match self
            .vfs
            .meta()
            .stale_directories(self.config.metadata_max_age, self.config.revalidate_batch)
            .await
        {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("could not list stale directories: {e}");
                return 0;
            }
        };

        let mut done = 0;
        for dir in stale {
            match self.vfs.revalidate(&dir).await {
                Ok(()) => done += 1,
                Err(RemoteError::Offline(_)) => {
                    // Stop the pass rather than grinding through the whole
                    // batch once the network has gone.
                    self.health.record_failure();
                    break;
                }
                Err(e) => tracing::debug!("revalidating {dir} failed: {e}"),
            }
        }
        done
    }

    /// Reclaim disk: superseded blobs, then orphans, then LRU.
    pub async fn gc_pass(&self) -> Result<Eviction, RemoteError> {
        let meta = self.vfs.meta();
        let content = self.vfs.content();
        let mut out = Eviction::default();

        // 1. Blobs on disk that no row points at. These can only come from a
        //    crash between the rename and the database write.
        let known: std::collections::HashSet<String> = meta
            .all_content_keys()
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();
        if let Ok(on_disk) = content.blob_keys() {
            for key in on_disk {
                if !known.contains(&key) {
                    let _ = content.remove(&key);
                    out.orphans += 1;
                }
            }
        }

        // 2. LRU, until under budget and above the free-space floor.
        let mut used = meta.total_content_bytes().await.unwrap_or(0);
        let free = free_bytes(content.root()).unwrap_or(u64::MAX);
        let need_space = free < self.config.cache_min_free_bytes;

        if used > self.config.cache_max_bytes || need_space {
            let target = if need_space {
                // Free half the budget when the disk itself is tight.
                self.config.cache_max_bytes / 2
            } else {
                self.config.cache_max_bytes
            };

            let candidates = meta.eviction_candidates(512).await.unwrap_or_default();
            for (id, _key, len) in candidates {
                if used <= target {
                    break;
                }
                // Database first, then the file: a row must never point at a
                // blob that is gone.
                if let Ok(Some(key)) = meta.clear_content(&id).await {
                    let _ = content.remove(&key);
                    out.evicted += 1;
                    used = used.saturating_sub(len);
                }
            }
        }

        Ok(out)
    }

    /// Probe the backend and update health.
    pub async fn probe(&self) -> bool {
        match self.vfs.remote().probe().await {
            Ok(()) => {
                if !self.health.is_online() {
                    tracing::info!("backend reachable again");
                }
                self.health.record_success();
                true
            }
            Err(e) => {
                if self.health.is_online() {
                    tracing::warn!("backend unreachable: {e}");
                }
                self.health.record_failure();
                false
            }
        }
    }

    /// How long to wait before the next probe while offline.
    ///
    /// Exponential, capped. A mount left offline overnight must not spend the
    /// night making connection attempts.
    pub fn offline_backoff(&self) -> Duration {
        let n = self.health.failures().min(16) as u32;
        self.config
            .offline_retry_min
            .saturating_mul(1u32 << n.min(10))
            .min(self.config.offline_retry_max)
    }

    /// The worker loop. Returns when `shutdown` fires.
    pub async fn run(self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let mut revalidate = tokio::time::interval(self.config.revalidate_interval);
        let mut gc = tokio::time::interval(self.config.gc_interval);
        revalidate.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        gc.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::debug!("sync worker stopping");
                        return;
                    }
                }
                _ = revalidate.tick() => {
                    if self.health.is_online() {
                        let n = self.revalidate_pass().await;
                        if n > 0 {
                            tracing::debug!("revalidated {n} directories");
                        }
                    } else {
                        // Offline: probe on a backoff instead.
                        tokio::time::sleep(self.offline_backoff()).await;
                        self.probe().await;
                    }
                }
                _ = gc.tick() => {
                    match self.gc_pass().await {
                        Ok(e) if e.evicted > 0 || e.orphans > 0 => {
                            tracing::info!(
                                "cache gc: {} evicted, {} orphans removed",
                                e.evicted, e.orphans
                            );
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!("cache gc failed: {e}"),
                    }
                }
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Maintenance {
    pub revalidated: usize,
    pub evicted: Eviction,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Eviction {
    pub evicted: usize,
    pub orphans: usize,
}

/// Free bytes on the filesystem holding `path`.
fn free_bytes(path: &std::path::Path) -> Option<u64> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // Safe: c_path is NUL-terminated and statvfs writes only into `buf`.
    unsafe {
        let mut buf: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut buf) != 0 {
            return None;
        }
        Some(buf.f_bavail.saturating_mul(buf.f_frsize))
    }
}

/// Startup housekeeping, run before the mount is exposed.
///
/// Deliberately startup-only: pruning while FUSE traffic is flowing races the
/// kernel's dentry cache, and bounding the database at startup is enough.
pub async fn startup_prune(vfs: &Arc<Vfs>, keep: Duration) -> Result<usize, RemoteError> {
    let removed = vfs.meta().prune_unlisted(keep).await.unwrap_or(0);
    if removed > 0 {
        tracing::info!("pruned {removed} stale directory listings");
    }
    Ok(removed)
}

/// Nodes whose directory listing has gone stale, for diagnostics.
pub async fn stale_count(vfs: &Arc<Vfs>, max_age: Duration) -> usize {
    vfs.meta()
        .stale_directories(max_age, i64::MAX)
        .await
        .map(|v| v.len())
        .unwrap_or(0)
}

/// Convenience for a caller that wants a health handle and a worker together.
pub fn spawn(
    vfs: Arc<Vfs>,
    config: SyncConfig,
) -> (
    Arc<Health>,
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<()>,
) {
    let health = Arc::new(Health::default());
    health.record_success(); // optimistic; the first probe corrects it
    let (tx, rx) = tokio::sync::watch::channel(false);
    let worker = SyncWorker::new(vfs, config, health.clone());
    let handle = tokio::spawn(worker.run(rx));
    (health, tx, handle)
}
