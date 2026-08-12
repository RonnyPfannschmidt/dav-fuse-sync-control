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
    _server: RcloneServer,
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
            _server: server,
        }
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
