//! Mount lifecycle: readiness, signals, watchdog, ordered shutdown.
//!
//! Used by both `davfs-sync mount` (foreground) and `davfs-sync daemon` (under
//! systemd). They differ only in where configuration comes from, so the thing
//! debugged interactively is the thing that runs in production.
//!
//! `sd_notify` no-ops without `$NOTIFY_SOCKET`, so no branching is needed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sd_notify::NotifyState;

use crate::fuse::{DavFS, OwnerIds};
use crate::remote::RemoteStorage;
use crate::store::{ContentCache, MetaStore};
use crate::sync::{Health, SyncConfig, SyncWorker};
use crate::vfs::{Vfs, VfsConfig};

/// FUSE_SUPER_MAGIC, for confirming the mount is actually live.
const FUSE_SUPER_MAGIC: i64 = 0x6573_5546;

pub struct MountParams {
    pub name: String,
    pub mount_point: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub backend_tag: String,
    pub vfs: VfsConfig,
    pub sync: SyncConfig,
    pub allow_other: bool,
    /// Event loop threads. Each gets its own fd when `clone_fd` is on.
    pub fuse_threads: usize,
    /// Depth of the crawl that runs *after* readiness.
    pub initial_sync_depth: usize,
}

impl MountParams {
    pub fn mount_options(&self) -> Vec<fuser::MountOption> {
        let mut o = vec![
            fuser::MountOption::FSName("davfs-sync".to_string()),
            fuser::MountOption::Subtype("davfs".to_string()),
            fuser::MountOption::NoAtime,
        ];
        // Belt to the EROFS braces in the FUSE layer: the kernel rejects writes
        // before they ever reach us.
        if self.vfs.read_only {
            o.push(fuser::MountOption::RO);
        }
        o
    }

    /// Who may access the mount.
    ///
    /// In fuser 0.18 this is `Config::acl`, not a mount option. `All` needs
    /// `user_allow_other` in /etc/fuse.conf.
    pub fn acl(&self) -> fuser::SessionACL {
        if self.allow_other {
            fuser::SessionACL::All
        } else {
            fuser::SessionACL::Owner
        }
    }
}

/// Build the VFS. Separated from `run` so tests can construct one without
/// mounting anything.
pub async fn build_vfs(
    remote: Arc<dyn RemoteStorage>,
    params: &MountParams,
) -> Result<(Arc<Vfs>, MetaStore)> {
    std::fs::create_dir_all(&params.data_dir)
        .with_context(|| format!("creating {}", params.data_dir.display()))?;
    std::fs::create_dir_all(&params.cache_dir)
        .with_context(|| format!("creating {}", params.cache_dir.display()))?;

    let db_path = format!("sqlite://{}", params.data_dir.join("metadata.db").display());
    let meta = MetaStore::new(&db_path, params.vfs.ttl).await?;
    meta.ensure_root(&remote.root(), &params.backend_tag)
        .await?;

    let content = ContentCache::new(&params.cache_dir)?;
    let vfs = Vfs::new(
        remote,
        meta.clone(),
        content,
        OwnerIds::current(),
        params.vfs.clone(),
    )
    .await?;

    Ok((vfs, meta))
}

/// Mount, serve, and shut down cleanly.
///
/// Ordering is the whole point of this function:
///
/// 1. The mount goes up **before** anything slow. The PoC probed the connection
///    and ran a recursive crawl first, so under `Type=notify` a large or
///    unreachable server meant a start timeout -- for a filesystem whose entire
///    selling point is working offline.
/// 2. `READY=1` fires once the kernel reports a FUSE superblock at the mount
///    point, which is the first moment the mount is actually usable.
/// 3. The crawl and the sync worker start afterwards, and report progress
///    through `STATUS=`.
pub async fn run(remote: Arc<dyn RemoteStorage>, params: MountParams) -> Result<()> {
    let (vfs, meta) = build_vfs(remote.clone(), &params).await?;

    std::fs::create_dir_all(&params.mount_point)
        .with_context(|| format!("creating {}", params.mount_point.display()))?;

    let fs = DavFS::new(tokio::runtime::Handle::current(), vfs.clone());

    let mut config = fuser::Config::default();
    config.mount_options = params.mount_options();
    config.acl = params.acl();
    // Several event loops, each with its own fd. Combined with handlers that
    // spawn rather than block, one slow request no longer stalls the mount.
    config.n_threads = Some(params.fuse_threads.max(1));
    config.clone_fd = true;

    // Session::new performs the mount syscall.
    let mut session = fuser::Session::new(fs, &params.mount_point, &config)
        .with_context(|| format!("mounting at {}", params.mount_point.display()))?;

    // MUST be taken before the session moves.
    //
    // Trap: BackgroundSession::new does a std::mem::take on the Mount, so an
    // unmounter obtained before spawn() silently unmounts nothing.
    // unmount_callable() composes with run(), not with spawn().
    let mut unmounter = session.unmount_callable();

    let loop_alive = Arc::new(AtomicBool::new(true));
    let thread_alive = loop_alive.clone();
    let session_thread = std::thread::Builder::new()
        .name("fuse-session".into())
        .spawn(move || {
            let result = session.run();
            thread_alive.store(false, Ordering::SeqCst);
            if let Err(e) = &result {
                tracing::debug!("fuse session ended: {e}");
            }
        })
        .context("spawning the FUSE session thread")?;

    // The superblock is not necessarily visible the instant Session::new
    // returns, so poll rather than assume.
    if !wait_for_mount(&params.mount_point, Duration::from_secs(10)) {
        let _ = unmounter.unmount();
        anyhow::bail!(
            "the mount at {} never appeared",
            params.mount_point.display()
        );
    }

    notify(&[
        NotifyState::Ready,
        NotifyState::Status(&format!("mounted {}", params.mount_point.display())),
    ]);
    tracing::info!("mounted at {}", params.mount_point.display());

    // Everything slow happens after readiness.
    let health = Arc::new(Health::default());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let warmup = {
        let vfs = vfs.clone();
        let health = health.clone();
        let depth = params.initial_sync_depth;
        let remote = remote.clone();
        tokio::spawn(async move {
            match remote.probe().await {
                Ok(()) => {
                    health.record_success();
                    notify(&[NotifyState::Status("online; syncing")]);
                }
                Err(e) => {
                    // Not fatal: serving cached data while offline is a
                    // legitimate ready state, and is the point of the project.
                    health.record_failure();
                    tracing::warn!("backend unreachable: {e}");
                    notify(&[NotifyState::Status(Health::default().status_line())]);
                    return;
                }
            }
            match vfs.initial_sync(depth).await {
                Ok(n) => tracing::info!("initial sync cached {n} directories"),
                Err(e) => tracing::warn!("initial sync failed: {e}"),
            }
            notify(&[NotifyState::Status("online")]);
        })
    };

    let worker = tokio::spawn(
        SyncWorker::new(vfs.clone(), params.sync.clone(), health.clone()).run(shutdown_rx.clone()),
    );
    let watchdog = spawn_watchdog(
        loop_alive.clone(),
        params.mount_point.clone(),
        health.clone(),
        shutdown_rx,
    );

    wait_for_signal(&loop_alive).await;

    // -- shutdown ---------------------------------------------------------
    notify(&[NotifyState::Stopping]);
    tracing::info!("shutting down");

    let _ = shutdown_tx.send(true);
    warmup.abort();
    watchdog.abort();

    // Makes run() return, which ends the session thread.
    if let Err(e) = unmounter.unmount() {
        tracing::warn!("unmount failed: {e}");
    }
    join_with_timeout(session_thread, Duration::from_secs(20));
    let _ = tokio::time::timeout(Duration::from_secs(5), worker).await;

    // Leaves SQLite checkpointed rather than mid-WAL.
    meta.close().await;
    tracing::info!("stopped");
    Ok(())
}

/// Block until SIGTERM, SIGINT, or the session thread exits on its own
/// (an external `fusermount -u`).
async fn wait_for_signal(loop_alive: &Arc<AtomicBool>) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("cannot listen for SIGTERM: {e}");
            return;
        }
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("cannot listen for SIGINT: {e}");
            return;
        }
    };
    // SIGHUP must be handled: its default disposition is *terminate*, so a
    // `systemctl --user reload` would otherwise kill the daemon outright.
    let mut hup = signal(SignalKind::hangup()).ok();

    loop {
        let hup_recv = async {
            match hup.as_mut() {
                Some(h) => {
                    h.recv().await;
                }
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            _ = term.recv() => { tracing::info!("SIGTERM"); return; }
            _ = int.recv()  => { tracing::info!("SIGINT"); return; }
            _ = hup_recv => {
                // Nothing to reload yet; explicitly a no-op rather than death.
                tracing::info!("SIGHUP ignored (no reloadable state)");
            }
            _ = async {
                while loop_alive.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            } => {
                tracing::info!("the mount went away; exiting");
                return;
            }
        }
    }
}

/// Ping the watchdog only while the mount is demonstrably healthy.
///
/// An unconditional ping is worse than no watchdog: it turns "hung filesystem"
/// into "systemd reports healthy". Health is checked without issuing a
/// filesystem operation -- that would send a FUSE request to ourselves and can
/// deadlock against the session loop.
fn spawn_watchdog(
    loop_alive: Arc<AtomicBool>,
    mount_point: PathBuf,
    health: Arc<Health>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut usec = 0u64;
        if !sd_notify::watchdog_enabled(false, &mut usec) || usec == 0 {
            return;
        }
        // systemd requires the ping well inside the interval.
        let period = Duration::from_micros(usec / 2);
        let mut tick = tokio::time::interval(period);

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                }
                _ = tick.tick() => {
                    let alive = loop_alive.load(Ordering::SeqCst);
                    let mounted = is_fuse_mount(&mount_point);
                    if alive && mounted {
                        notify(&[NotifyState::Watchdog]);
                    } else {
                        // Say nothing and let systemd act.
                        tracing::error!(
                            "unhealthy (session alive: {alive}, mounted: {mounted}); \
                             withholding watchdog ping"
                        );
                    }
                    let _ = &health;
                }
            }
        }
    })
}

fn join_with_timeout(handle: std::thread::JoinHandle<()>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if handle.is_finished() {
            let _ = handle.join();
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    tracing::warn!("the FUSE session thread did not exit within {timeout:?}");
}

pub fn is_fuse_mount(path: &Path) -> bool {
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // Safe: c_path is NUL-terminated and statfs writes only into `buf`.
    unsafe {
        let mut buf: libc::statfs = std::mem::zeroed();
        if libc::statfs(c_path.as_ptr(), &mut buf) != 0 {
            return false;
        }
        buf.f_type == FUSE_SUPER_MAGIC
    }
}

pub fn wait_for_mount(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if is_fuse_mount(path) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Notify systemd, ignoring the error when there is no socket.
fn notify(states: &[NotifyState]) {
    if let Err(e) = sd_notify::notify(false, states) {
        tracing::trace!("sd_notify: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(read_only: bool, allow_other: bool) -> MountParams {
        MountParams {
            name: "t".into(),
            mount_point: "/tmp/x".into(),
            data_dir: "/tmp/d".into(),
            cache_dir: "/tmp/c".into(),
            backend_tag: "webdav".into(),
            vfs: VfsConfig {
                read_only,
                ..Default::default()
            },
            sync: SyncConfig::default(),
            allow_other,
            fuse_threads: 4,
            initial_sync_depth: 2,
        }
    }

    #[test]
    fn a_read_only_mount_asks_the_kernel_to_enforce_it() {
        let o = params(true, false).mount_options();
        assert!(o.contains(&fuser::MountOption::RO));
    }

    #[test]
    fn a_writable_mount_does_not_pass_ro() {
        let o = params(false, false).mount_options();
        assert!(!o.contains(&fuser::MountOption::RO));
    }

    #[test]
    fn allow_other_is_opt_in() {
        assert_eq!(params(true, false).acl(), fuser::SessionACL::Owner);
        assert_eq!(params(true, true).acl(), fuser::SessionACL::All);
    }

    #[test]
    fn the_filesystem_is_identifiable_in_mount_output() {
        let o = params(true, false).mount_options();
        assert!(o.contains(&fuser::MountOption::FSName("davfs-sync".into())));
        assert!(o.contains(&fuser::MountOption::Subtype("davfs".into())));
    }

    #[test]
    fn an_empty_directory_is_not_mistaken_for_a_mount() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_fuse_mount(dir.path()));
        assert!(!is_fuse_mount(Path::new("/nonexistent-path-xyz")));
    }
}
