//! A real Nextcloud, when one is available.
//!
//! Nextcloud (SabreDAV) differs from rclone in exactly the places this project
//! changed: the `d:` namespace prefix, a deep base path
//! (`/remote.php/dav/files/<user>/`), `getetag` on collections, a second
//! `404 Not Found` propstat for unsupported properties, and real conditional
//! request handling. Testing only against rclone would leave all of that
//! unverified.
//!
//! Configured by environment, so the same tests run against a CI service
//! container and against a local `podman run nextcloud`.

pub const ENV_URL: &str = "DAVFS_NEXTCLOUD_URL";
pub const ENV_USER: &str = "DAVFS_NEXTCLOUD_USER";
pub const ENV_PASS: &str = "DAVFS_NEXTCLOUD_PASS";

pub struct Nextcloud {
    pub base: String,
    pub user: String,
    pub pass: String,
}

impl Nextcloud {
    /// Read the connection details from the environment.
    ///
    /// `DAVFS_NEXTCLOUD_URL` may be either the server root or the full WebDAV
    /// path; the per-user WebDAV path is appended when it is missing, so both
    /// `http://host:8080` and `http://host:8080/remote.php/dav/files/admin/`
    /// work.
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var(ENV_URL).ok()?;
        let user = std::env::var(ENV_USER).unwrap_or_else(|_| "admin".to_string());
        let pass = std::env::var(ENV_PASS).unwrap_or_else(|_| "adminpass123".to_string());

        let trimmed = raw.trim_end_matches('/');
        let base = if trimmed.contains("/remote.php/dav/") {
            format!("{trimmed}/")
        } else {
            format!("{trimmed}/remote.php/dav/files/{user}/")
        };

        Some(Self { base, user, pass })
    }

    /// Upload a file with `curl`, so test fixtures do not depend on the code
    /// under test being able to write.
    pub fn put(&self, remote_path: &str, body: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new()?;
        tmp.write_all(body)?;
        tmp.flush()?;

        let url = format!("{}{}", self.base, encode_path(remote_path));
        let out = std::process::Command::new("curl")
            .args(["-sS", "-o", "/dev/null", "-w", "%{http_code}"])
            .args(["-u", &format!("{}:{}", self.user, self.pass)])
            .args(["-X", "PUT", "--data-binary"])
            .arg(format!("@{}", tmp.path().display()))
            .arg(&url)
            .output()?;

        let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !code.starts_with('2') {
            return Err(std::io::Error::other(format!(
                "PUT {remote_path} -> HTTP {code}"
            )));
        }
        Ok(())
    }

    pub fn mkcol(&self, remote_path: &str) -> std::io::Result<()> {
        let url = format!("{}{}", self.base, encode_path(remote_path));
        let out = std::process::Command::new("curl")
            .args(["-sS", "-o", "/dev/null", "-w", "%{http_code}"])
            .args(["-u", &format!("{}:{}", self.user, self.pass)])
            .args(["-X", "MKCOL"])
            .arg(&url)
            .output()?;
        let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // 405 means it already exists, which is fine for a fixture.
        if !code.starts_with('2') && code != "405" {
            return Err(std::io::Error::other(format!(
                "MKCOL {remote_path} -> HTTP {code}"
            )));
        }
        Ok(())
    }

    pub fn delete(&self, remote_path: &str) -> std::io::Result<()> {
        let url = format!("{}{}", self.base, encode_path(remote_path));
        let _ = std::process::Command::new("curl")
            .args(["-sS", "-o", "/dev/null"])
            .args(["-u", &format!("{}:{}", self.user, self.pass)])
            .args(["-X", "DELETE"])
            .arg(&url)
            .output()?;
        Ok(())
    }
}

/// Percent-encode each path segment for a URL.
fn encode_path(path: &str) -> String {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|seg| {
            seg.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Skip unless a Nextcloud is configured, but fail when CI says one must be.
#[macro_export]
macro_rules! require_nextcloud {
    () => {{
        match $crate::common::nextcloud::Nextcloud::from_env() {
            Some(nc) => nc,
            None => {
                if std::env::var_os("DAVFS_NEXTCLOUD_REQUIRE").is_some() {
                    panic!(
                        "DAVFS_NEXTCLOUD_REQUIRE is set but {} is not",
                        $crate::common::nextcloud::ENV_URL
                    );
                }
                eprintln!("SKIP {}: no Nextcloud configured", module_path!());
                return;
            }
        }
    }};
}
