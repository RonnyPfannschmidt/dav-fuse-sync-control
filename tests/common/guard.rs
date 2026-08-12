//! A FUSE mount that always tears itself down.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::rclone::RcloneServer;

/// FUSE_SUPER_MAGIC. Used to tell "mounted" from "an empty directory".
const FUSE_SUPER_MAGIC: i64 = 0x6573_5546;

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

/// Unmounts on drop, including while unwinding from a panic.
///
/// Field order is the drop order and matters: the session must go before the
/// rclone server, or the FUSE thread can block on a dead socket during
/// teardown.
pub struct MountGuard {
    session: Option<fuser::BackgroundSession>,
    mountpoint: TempDir,
    /// Only set when the server is ours to shut down; a Nextcloud container
    /// outlives the test.
    _server: Option<RcloneServer>,
    /// Directories that must outlive the mount: the metadata database, the
    /// blob cache, and the fixture tree the server is serving.
    ///
    /// They belong here rather than being leaked with `std::mem::forget`.
    /// Leaking them left ~3 directories per test behind in /tmp -- including an
    /// 8 MiB fixture file each -- which reached 1.7 GB over a day's runs.
    _dirs: Vec<TempDir>,
}

impl MountGuard {
    pub fn new(
        session: fuser::BackgroundSession,
        mountpoint: TempDir,
        server: RcloneServer,
    ) -> Self {
        Self {
            session: Some(session),
            mountpoint,
            _server: Some(server),
            _dirs: Vec::new(),
        }
    }

    /// For a server the test does not own, such as a Nextcloud container.
    pub fn without_server(session: fuser::BackgroundSession, mountpoint: TempDir) -> Self {
        Self {
            session: Some(session),
            mountpoint,
            _server: None,
            _dirs: Vec::new(),
        }
    }

    /// Hand the guard ownership of directories that must outlive the mount.
    pub fn owning(mut self, dirs: Vec<TempDir>) -> Self {
        self._dirs = dirs;
        self
    }

    pub fn path(&self) -> PathBuf {
        self.mountpoint.path().to_path_buf()
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        let mp = self.mountpoint.path().to_owned();

        // Dropping BackgroundSession drops its Mount, which unmounts.
        drop(self.session.take());

        for _ in 0..50 {
            if !is_fuse_mount(&mp) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Unwinding covers panics; a wedged session needs the lazy fallback.
        if is_fuse_mount(&mp) {
            let _ = Command::new("fusermount3")
                .args(["-uz", &mp.to_string_lossy()])
                .status();
        }
        // TempDir::drop now runs against an unmounted directory.
    }
}
