//! The fixture tree served to integration tests.
//!
//! Built at runtime, never committed: git normalises filenames and mtimes, and
//! several of these names are hostile to a Windows checkout.

use std::path::Path;
use tempfile::TempDir;

/// A file whose mtime is pinned, so mtime assertions are exact.
pub const KNOWN_MTIME_FILE: &str = "known-mtime.txt";
pub const KNOWN_MTIME_UNIX: i64 = 1_583_298_367; // 2020-03-04T05:06:07Z

/// Size of `large.bin`, enough to span many 128 KiB FUSE reads.
pub const LARGE_LEN: usize = 8 * 1024 * 1024;

pub fn build() -> std::io::Result<TempDir> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();

    std::fs::write(root.join("plain.txt"), b"hello\n")?;
    std::fs::write(root.join("spaces in name.txt"), b"spaces\n")?;
    std::fs::write(root.join("ümlaut-Ä.txt"), b"umlaut\n")?;
    std::fs::write(root.join("emoji-🚀.md"), b"rocket\n")?;
    // '#' and '&' are the characters Url::join used to truncate.
    std::fs::write(root.join("hash#and&.txt"), b"hash\n")?;
    std::fs::write(root.join("apostrophe's.txt"), b"quote\n")?;
    // A literal percent must survive percent-decoding unharmed.
    std::fs::write(root.join("100%done.txt"), b"percent\n")?;

    std::fs::create_dir(root.join("empty"))?;
    std::fs::create_dir_all(root.join("nested/a/b/c"))?;
    std::fs::write(root.join("nested/a/b/c/deep.txt"), b"deep\n")?;

    // Deterministic content, so a reader can verify every byte.
    let large: Vec<u8> = (0..LARGE_LEN).map(|i| (i % 251) as u8).collect();
    std::fs::write(root.join("large.bin"), &large)?;

    let known = root.join(KNOWN_MTIME_FILE);
    std::fs::write(&known, b"pinned\n")?;
    set_mtime(&known, KNOWN_MTIME_UNIX)?;

    Ok(dir)
}

/// Expected top-level names, sorted.
pub fn expected_root_names() -> Vec<&'static str> {
    let mut v = vec![
        "100%done.txt",
        "apostrophe's.txt",
        "emoji-🚀.md",
        "empty",
        "hash#and&.txt",
        KNOWN_MTIME_FILE,
        "large.bin",
        "nested",
        "plain.txt",
        "spaces in name.txt",
        "ümlaut-Ä.txt",
    ];
    v.sort_unstable();
    v
}

pub fn large_content() -> Vec<u8> {
    (0..LARGE_LEN).map(|i| (i % 251) as u8).collect()
}

fn set_mtime(path: &Path, unix: i64) -> std::io::Result<()> {
    let times = [
        libc::timespec {
            tv_sec: unix,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: unix,
            tv_nsec: 0,
        },
    ];
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // Safe: c_path is NUL-terminated and times has the required two elements.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
