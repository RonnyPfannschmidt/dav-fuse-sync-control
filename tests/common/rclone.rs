//! A real WebDAV server for integration tests, via `rclone serve webdav`.
//!
//! rclone is a genuinely independent implementation: it uses the `D:` namespace
//! prefix, emits the whole document on one line, and percent-encodes hrefs.
//! Testing against it is what caught the parser returning zero entries.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const USER: &str = "testuser";
pub const PASS: &str = "testpass";

pub struct RcloneServer {
    child: Child,
    pub base: url::Url,
    /// stderr, kept so a failing test can show what the server said.
    log: Arc<Mutex<String>>,
}

impl RcloneServer {
    /// Is rclone available? Tests skip rather than fail when it is not.
    pub fn available() -> bool {
        Command::new("rclone")
            .arg("version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Serve `root` on an ephemeral loopback port.
    pub fn start(root: &std::path::Path) -> std::io::Result<Self> {
        let mut child = Command::new("rclone")
            .args([
                "serve",
                "webdav",
                "--addr",
                "127.0.0.1:0",
                "--user",
                USER,
                "--pass",
                PASS,
            ])
            .arg(root)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;

        let stderr = child.stderr.take().expect("stderr piped");
        let log = Arc::new(Mutex::new(String::new()));

        // rclone logs the bound address to stderr at NOTICE level, without -v:
        //   NOTICE: ...: WebDav Server started on [http://127.0.0.1:39923/]
        // Match the bare address so a wording change upstream does not break CI.
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        {
            let log = log.clone();
            std::thread::spawn(move || {
                let mut sent = false;
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if !sent {
                        if let Some(addr) = extract_addr(&line) {
                            let _ = tx.send(addr);
                            sent = true;
                        }
                    }
                    // Keep draining, or rclone blocks once the pipe fills.
                    log.lock().unwrap().push_str(&line);
                    log.lock().unwrap().push('\n');
                }
            });
        }

        let addr = rx.recv_timeout(Duration::from_secs(15)).map_err(|_| {
            let _ = child.kill();
            std::io::Error::other(format!(
                "rclone did not report a listen address within 15s. stderr:\n{}",
                log.lock().unwrap()
            ))
        })?;

        let base = url::Url::parse(&addr).expect("rclone printed a valid URL");
        let server = Self { child, base, log };
        server.wait_ready(Duration::from_secs(10))?;
        Ok(server)
    }

    /// Poll until the server answers, rather than sleeping a guessed interval.
    fn wait_ready(&self, timeout: Duration) -> std::io::Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let out = Command::new("curl")
                .args(["-sS", "-o", "/dev/null", "-w", "%{http_code}"])
                .args(["-X", "PROPFIND", "-H", "Depth: 0"])
                .args(["-u", &format!("{USER}:{PASS}")])
                .arg(self.base.as_str())
                .output();
            if let Ok(out) = out {
                let code = String::from_utf8_lossy(&out.stdout);
                // 207 Multi-Status when it is up; 401 also proves it is
                // listening, which is enough for readiness.
                if code == "207" || code == "401" {
                    return Ok(());
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(std::io::Error::other(format!(
            "rclone did not become ready. stderr:\n{}",
            self.log.lock().unwrap()
        )))
    }

    pub fn stderr(&self) -> String {
        self.log.lock().unwrap().clone()
    }
}

impl Drop for RcloneServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn extract_addr(line: &str) -> Option<String> {
    let start = line.find("http://127.0.0.1:")?;
    let rest = &line[start..];
    let end = rest.find([']', ' ', '\n']).unwrap_or(rest.len());
    let mut addr = rest[..end].to_string();
    if !addr.ends_with('/') {
        addr.push('/');
    }
    Some(addr)
}

#[cfg(test)]
mod tests {
    use super::extract_addr;

    #[test]
    fn parses_the_notice_line() {
        let line = "2026/08/12 08:37:07 NOTICE: Local file system at /tmp/x: \
                    WebDav Server started on [http://127.0.0.1:39923/]";
        assert_eq!(
            extract_addr(line).as_deref(),
            Some("http://127.0.0.1:39923/")
        );
        assert_eq!(extract_addr("nothing here"), None);
    }
}
