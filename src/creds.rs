//! Where the password comes from.
//!
//! An enum rather than a trait: no `async_trait`, no vtable, and the `Static`
//! variant is trivially constructible in tests. This is the seam that lets the
//! test suite and CI run without a D-Bus session or a keyring.
//!
//! spec/02 sketches this as `enum StorageBackend`; this follows that shape.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Environment variable that pins the source explicitly, bypassing detection.
pub const ENV_SOURCE: &str = "DAVFS_CREDENTIAL";
pub const ENV_PASSWORD: &str = "DAVFS_PASSWORD";
pub const ENV_PASSWORD_FILE: &str = "DAVFS_PASSWORD_FILE";
/// Set by systemd for units using LoadCredential=/LoadCredentialEncrypted=.
pub const ENV_CREDENTIALS_DIR: &str = "CREDENTIALS_DIRECTORY";

#[derive(Debug, Clone)]
pub enum CredentialSource {
    /// systemd `LoadCredential[Encrypted]=` -- read `$CREDENTIALS_DIRECTORY/password`.
    Systemd { dir: PathBuf },
    /// A file containing the password, trailing newline trimmed.
    File(PathBuf),
    /// `$DAVFS_PASSWORD`. For containers and CI.
    Env,
    /// The session keyring.
    SecretService,
    /// Tests.
    Static(String),
}

/// Read-only view of the process environment, injectable so tests need not
/// mutate the process-global environment (which makes parallel tests flaky).
pub trait EnvSource: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
}

/// The real process environment.
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// A fixed map, for tests.
#[derive(Default)]
pub struct MapEnv(pub std::collections::HashMap<String, String>);

impl MapEnv {
    pub fn from<const N: usize>(pairs: [(&str, &str); N]) -> Self {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }
}

impl EnvSource for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

impl CredentialSource {
    /// Pick a source from the environment.
    ///
    /// `DAVFS_CREDENTIAL` pins it explicitly. Otherwise detect in order:
    /// systemd credentials, password file, password variable, keyring.
    ///
    /// systemd wins when present because it is authoritative for that unit.
    /// Secret Service is last because it is the one that can block on a locked
    /// keyring.
    pub fn detect(env: &dyn EnvSource) -> Result<Self> {
        if let Some(explicit) = env.get(ENV_SOURCE) {
            return match explicit.as_str() {
                "systemd" => {
                    let dir = env.get(ENV_CREDENTIALS_DIR).context(
                        "DAVFS_CREDENTIAL=systemd but CREDENTIALS_DIRECTORY is not set; \
                         the unit needs LoadCredential= or LoadCredentialEncrypted=",
                    )?;
                    Ok(Self::Systemd { dir: dir.into() })
                }
                "file" => {
                    let p = env
                        .get(ENV_PASSWORD_FILE)
                        .context("DAVFS_CREDENTIAL=file but DAVFS_PASSWORD_FILE is not set")?;
                    Ok(Self::File(p.into()))
                }
                "env" => Ok(Self::Env),
                "secret-service" => Ok(Self::SecretService),
                other => anyhow::bail!(
                    "unknown {ENV_SOURCE}={other:?}; \
                     expected one of: systemd, file, env, secret-service"
                ),
            };
        }

        if let Some(dir) = env.get(ENV_CREDENTIALS_DIR) {
            return Ok(Self::Systemd { dir: dir.into() });
        }
        if let Some(p) = env.get(ENV_PASSWORD_FILE) {
            return Ok(Self::File(p.into()));
        }
        if env.get(ENV_PASSWORD).is_some() {
            return Ok(Self::Env);
        }
        Ok(Self::SecretService)
    }

    /// Whether resolving this source needs D-Bus. Tests and CI assert this is
    /// false for everything they use.
    pub fn needs_dbus(&self) -> bool {
        matches!(self, Self::SecretService)
    }

    /// Resolve the password for `mount`.
    pub async fn password(&self, mount: &str, env: &dyn EnvSource) -> Result<String> {
        match self {
            Self::Systemd { dir } => read_trimmed(&dir.join("password"))
                .with_context(|| format!("reading systemd credential from {}", dir.display())),
            Self::File(path) => read_trimmed(path)
                .with_context(|| format!("reading password file {}", path.display())),
            Self::Env => env
                .get(ENV_PASSWORD)
                .context("DAVFS_PASSWORD is not set")
                .map(|s| s.to_string()),
            Self::SecretService => {
                let store = crate::secrets::SecretStore::new().await?;
                store.load_password(mount).await
            }
            Self::Static(s) => Ok(s.clone()),
        }
    }
}

/// Read a secret from a file, trimming exactly one trailing newline.
///
/// Trimming only the line ending, not all whitespace: a password may
/// legitimately begin or end with a space.
fn read_trimmed(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)?;
    let trimmed = raw.strip_suffix('\n').unwrap_or(&raw);
    let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_prefers_systemd_over_everything() {
        let env = MapEnv::from([
            (ENV_CREDENTIALS_DIR, "/run/creds"),
            (ENV_PASSWORD_FILE, "/tmp/pw"),
            (ENV_PASSWORD, "inline"),
        ]);
        assert!(matches!(
            CredentialSource::detect(&env).unwrap(),
            CredentialSource::Systemd { .. }
        ));
    }

    #[test]
    fn detect_falls_back_to_secret_service() {
        let env = MapEnv::default();
        let src = CredentialSource::detect(&env).unwrap();
        assert!(matches!(src, CredentialSource::SecretService));
        assert!(src.needs_dbus());
    }

    #[test]
    fn detect_honours_explicit_override() {
        let env = MapEnv::from([(ENV_SOURCE, "env"), (ENV_CREDENTIALS_DIR, "/run/creds")]);
        // Explicit wins even though systemd credentials are present.
        assert!(matches!(
            CredentialSource::detect(&env).unwrap(),
            CredentialSource::Env
        ));
    }

    #[test]
    fn explicit_systemd_without_directory_is_an_error() {
        let env = MapEnv::from([(ENV_SOURCE, "systemd")]);
        assert!(CredentialSource::detect(&env).is_err());
    }

    #[test]
    fn unknown_source_names_the_valid_options() {
        let env = MapEnv::from([(ENV_SOURCE, "keyring")]);
        let err = CredentialSource::detect(&env).unwrap_err().to_string();
        assert!(err.contains("secret-service"), "unhelpful message: {err}");
    }

    #[tokio::test]
    async fn file_source_trims_only_the_line_ending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pw");
        // Leading and trailing spaces are part of the password; the newline is not.
        std::fs::write(&path, " hunter2 \n").unwrap();

        let src = CredentialSource::File(path);
        let got = src.password("m", &MapEnv::default()).await.unwrap();
        assert_eq!(got, " hunter2 ");
        assert!(!src.needs_dbus());
    }

    #[tokio::test]
    async fn systemd_source_reads_the_password_credential() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("password"), "from-systemd").unwrap();

        let src = CredentialSource::Systemd {
            dir: dir.path().to_path_buf(),
        };
        assert_eq!(
            src.password("m", &MapEnv::default()).await.unwrap(),
            "from-systemd"
        );
    }

    #[tokio::test]
    async fn env_and_static_sources_resolve() {
        let env = MapEnv::from([(ENV_PASSWORD, "inline-pw")]);
        assert_eq!(
            CredentialSource::Env.password("m", &env).await.unwrap(),
            "inline-pw"
        );
        assert_eq!(
            CredentialSource::Static("t".into())
                .password("m", &MapEnv::default())
                .await
                .unwrap(),
            "t"
        );
    }
}
