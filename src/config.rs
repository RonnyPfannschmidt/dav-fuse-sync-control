//! Mount configuration.
//!
//! Three sources exist: the JSON blob `setup` stores in the Secret Service, the
//! `DAVFS_*` environment variables systemd needs, and CLI flags. Environment is
//! the runtime source of truth; stored configuration supplies the defaults.
//!
//! Every field added since v1 is `#[serde(default)]`, so a blob written by the
//! original version still deserialises. That is not a nicety: those blobs are
//! in users' keyrings and cannot be migrated remotely.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::creds::EnvSource;
use crate::sync::SyncConfig;
use crate::vfs::{OnOpenUncached, VfsConfig};

/// Which backend a mount speaks to.
///
/// Internally tagged, so adding a variant with fields never breaks an older
/// blob. `#[serde(default)]` plus `Default` means a v1 blob with no `backend`
/// key deserialises to `WebDav`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Backend {
    #[default]
    WebDav,
    GoogleDrive {
        #[serde(default)]
        root_folder_id: Option<String>,
        #[serde(default)]
        export_formats: Vec<String>,
    },
}

impl Backend {
    /// Stored in the cache database so a mount repointed at a different backend
    /// is detected rather than silently serving the wrong tree.
    pub fn tag(&self) -> &'static str {
        match self {
            Backend::WebDav => "webdav",
            Backend::GoogleDrive { .. } => "gdrive",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountConfig {
    pub name: String,

    // v1 field names, byte-identical, so a stored blob still parses.
    // `url` and `username` stay at the top level rather than moving into
    // `Backend`: nesting them would break every existing keyring entry for no
    // functional gain. For Drive, `url` is unused and `username` is the account.
    pub url: String,
    pub username: String,
    pub mount_point: PathBuf,

    #[serde(default)]
    pub backend: Backend,
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
    #[serde(default)]
    pub data_dir: Option<PathBuf>,

    #[serde(default = "d_ttl_secs")]
    pub metadata_ttl_secs: u64,
    #[serde(default = "d_cache_max_gb")]
    pub cache_max_gb: f64,
    #[serde(default = "d_min_free_gb")]
    pub cache_min_free_gb: f64,
    #[serde(default)]
    pub on_open_uncached: OnOpenUncached,
    #[serde(default = "d_refresh_min")]
    pub metadata_refresh_min: u64,
    #[serde(default = "d_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "d_timeout")]
    pub timeout_sec: u64,
    #[serde(default = "d_retries")]
    pub retry_count: u32,
    #[serde(default = "d_true")]
    pub read_only: bool,
    #[serde(default)]
    pub allow_other: bool,
    #[serde(default = "d_fuse_threads")]
    pub fuse_threads: usize,
    #[serde(default = "d_sync_depth")]
    pub initial_sync_depth: usize,
}

fn d_ttl_secs() -> u64 {
    30
}
fn d_cache_max_gb() -> f64 {
    10.0
}
fn d_min_free_gb() -> f64 {
    5.0
}
fn d_refresh_min() -> u64 {
    15
}
fn d_max_concurrent() -> usize {
    4
}
fn d_timeout() -> u64 {
    30
}
fn d_retries() -> u32 {
    3
}
fn d_true() -> bool {
    true
}
fn d_fuse_threads() -> usize {
    4
}
fn d_sync_depth() -> usize {
    2
}

impl MountConfig {
    /// A minimal WebDAV mount with defaults everywhere else.
    pub fn new(name: String, url: String, username: String, mount_point: PathBuf) -> Self {
        Self {
            name,
            url,
            username,
            mount_point,
            backend: Backend::default(),
            cache_dir: None,
            data_dir: None,
            metadata_ttl_secs: d_ttl_secs(),
            cache_max_gb: d_cache_max_gb(),
            cache_min_free_gb: d_min_free_gb(),
            on_open_uncached: OnOpenUncached::default(),
            metadata_refresh_min: d_refresh_min(),
            max_concurrent: d_max_concurrent(),
            timeout_sec: d_timeout(),
            retry_count: d_retries(),
            read_only: d_true(),
            allow_other: false,
            fuse_threads: d_fuse_threads(),
            initial_sync_depth: d_sync_depth(),
        }
    }

    pub fn data_dir(&self) -> Result<PathBuf> {
        Ok(match &self.data_dir {
            Some(p) => p.clone(),
            None => dirs::data_local_dir()
                .context("no XDG data directory")?
                .join("davfs-sync")
                .join(&self.name),
        })
    }

    pub fn cache_dir(&self) -> Result<PathBuf> {
        Ok(match &self.cache_dir {
            Some(p) => p.clone(),
            None => dirs::cache_dir()
                .context("no XDG cache directory")?
                .join("davfs-sync")
                .join(&self.name),
        })
    }

    pub fn vfs_config(&self) -> VfsConfig {
        VfsConfig {
            ttl: Duration::from_secs(self.metadata_ttl_secs),
            attr_ttl: Duration::from_secs(self.metadata_ttl_secs),
            on_open_uncached: self.on_open_uncached,
            max_concurrent: self.max_concurrent.max(1),
            max_inflight: 256,
            open_timeout: Duration::from_secs(self.timeout_sec),
            read_only: self.read_only,
        }
    }

    pub fn sync_config(&self) -> SyncConfig {
        let gb = |n: f64| (n * 1024.0 * 1024.0 * 1024.0) as u64;
        SyncConfig {
            revalidate_interval: Duration::from_secs(60),
            metadata_max_age: Duration::from_secs(self.metadata_refresh_min * 60),
            revalidate_batch: 32,
            gc_interval: Duration::from_secs(300),
            cache_max_bytes: gb(self.cache_max_gb),
            cache_min_free_bytes: gb(self.cache_min_free_gb),
            offline_retry_min: Duration::from_secs(5),
            offline_retry_max: Duration::from_secs(300),
        }
    }

    /// The `DAVFS_*` variables describing this mount, for a systemd drop-in.
    pub fn to_env(&self) -> BTreeMap<String, String> {
        let mut v = BTreeMap::new();
        v.insert("DAVFS_URL".into(), self.url.clone());
        v.insert("DAVFS_USERNAME".into(), self.username.clone());
        v.insert(
            "DAVFS_MOUNT_POINT".into(),
            self.mount_point.display().to_string(),
        );
        v.insert("DAVFS_BACKEND".into(), self.backend.tag().into());
        v.insert(
            "DAVFS_METADATA_TTL_SEC".into(),
            self.metadata_ttl_secs.to_string(),
        );
        v.insert("DAVFS_CACHE_MAX_GB".into(), self.cache_max_gb.to_string());
        v.insert(
            "DAVFS_CACHE_MIN_FREE_GB".into(),
            self.cache_min_free_gb.to_string(),
        );
        v.insert(
            "DAVFS_ON_OPEN_UNCACHED".into(),
            on_open_to_str(self.on_open_uncached).into(),
        );
        v.insert(
            "DAVFS_METADATA_REFRESH_MIN".into(),
            self.metadata_refresh_min.to_string(),
        );
        v.insert(
            "DAVFS_MAX_CONCURRENT".into(),
            self.max_concurrent.to_string(),
        );
        v.insert("DAVFS_TIMEOUT_SEC".into(), self.timeout_sec.to_string());
        v.insert("DAVFS_RETRY_COUNT".into(), self.retry_count.to_string());
        v.insert("DAVFS_READ_ONLY".into(), self.read_only.to_string());
        v.insert("DAVFS_ALLOW_OTHER".into(), self.allow_other.to_string());
        v.insert("DAVFS_FUSE_THREADS".into(), self.fuse_threads.to_string());
        v.insert(
            "DAVFS_INITIAL_SYNC_DEPTH".into(),
            self.initial_sync_depth.to_string(),
        );
        if let Some(p) = &self.data_dir {
            v.insert("DAVFS_DATA_DIR".into(), p.display().to_string());
        }
        if let Some(p) = &self.cache_dir {
            v.insert("DAVFS_CACHE_DIR".into(), p.display().to_string());
        }
        v
    }

    /// Build from `DAVFS_*` variables. The systemd path.
    pub fn from_env(env: &dyn EnvSource) -> Result<Self> {
        let name = env
            .get("DAVFS_NAME")
            .context("DAVFS_NAME is not set (the unit template sets it from %i)")?;
        let url = env.get("DAVFS_URL").context("DAVFS_URL is not set")?;
        let username = env.get("DAVFS_USERNAME").unwrap_or_default();
        let mount_point = env
            .get("DAVFS_MOUNT_POINT")
            .context("DAVFS_MOUNT_POINT is not set")?;

        let mut cfg = Self::new(name, url, username, PathBuf::from(mount_point));
        cfg.apply_env(env);
        Ok(cfg)
    }

    /// Overlay any `DAVFS_*` variables that are present.
    ///
    /// Shared by `from_env` and the stored-config path, so a value means the
    /// same thing however the process was started.
    pub fn apply_env(&mut self, env: &dyn EnvSource) {
        if let Some(v) = env.get("DAVFS_MOUNT_POINT") {
            self.mount_point = PathBuf::from(v);
        }
        if let Some(v) = env.get("DAVFS_DATA_DIR") {
            self.data_dir = Some(PathBuf::from(v));
        }
        if let Some(v) = env.get("DAVFS_CACHE_DIR") {
            self.cache_dir = Some(PathBuf::from(v));
        }
        if let Some(v) = env
            .get("DAVFS_METADATA_TTL_SEC")
            .and_then(|v| v.parse().ok())
        {
            self.metadata_ttl_secs = v;
        }
        if let Some(v) = env.get("DAVFS_CACHE_MAX_GB").and_then(|v| v.parse().ok()) {
            self.cache_max_gb = v;
        }
        if let Some(v) = env
            .get("DAVFS_CACHE_MIN_FREE_GB")
            .and_then(|v| v.parse().ok())
        {
            self.cache_min_free_gb = v;
        }
        if let Some(v) = env.get("DAVFS_ON_OPEN_UNCACHED") {
            if let Some(p) = on_open_from_str(&v) {
                self.on_open_uncached = p;
            }
        }
        if let Some(v) = env
            .get("DAVFS_METADATA_REFRESH_MIN")
            .and_then(|v| v.parse().ok())
        {
            self.metadata_refresh_min = v;
        }
        if let Some(v) = env.get("DAVFS_MAX_CONCURRENT").and_then(|v| v.parse().ok()) {
            self.max_concurrent = v;
        }
        if let Some(v) = env.get("DAVFS_TIMEOUT_SEC").and_then(|v| v.parse().ok()) {
            self.timeout_sec = v;
        }
        if let Some(v) = env.get("DAVFS_RETRY_COUNT").and_then(|v| v.parse().ok()) {
            self.retry_count = v;
        }
        if let Some(v) = env.get("DAVFS_READ_ONLY").and_then(|v| parse_bool(&v)) {
            self.read_only = v;
        }
        if let Some(v) = env.get("DAVFS_ALLOW_OTHER").and_then(|v| parse_bool(&v)) {
            self.allow_other = v;
        }
        if let Some(v) = env.get("DAVFS_FUSE_THREADS").and_then(|v| v.parse().ok()) {
            self.fuse_threads = v;
        }
        if let Some(v) = env
            .get("DAVFS_INITIAL_SYNC_DEPTH")
            .and_then(|v| v.parse().ok())
        {
            self.initial_sync_depth = v;
        }
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn on_open_to_str(p: OnOpenUncached) -> &'static str {
    match p {
        OnOpenUncached::Error => "error",
        OnOpenUncached::Block => "block",
        OnOpenUncached::Background => "background",
    }
}

fn on_open_from_str(s: &str) -> Option<OnOpenUncached> {
    match s.trim().to_ascii_lowercase().as_str() {
        "error" => Some(OnOpenUncached::Error),
        "block" => Some(OnOpenUncached::Block),
        "background" => Some(OnOpenUncached::Background),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::MapEnv;

    fn sample() -> MountConfig {
        MountConfig::new(
            "personal".into(),
            "https://cloud.example.com/remote.php/dav/files/alice/".into(),
            "alice".into(),
            "/home/alice/Cloud".into(),
        )
    }

    /// A blob written by the original version must still parse. Those live in
    /// users' keyrings and cannot be migrated remotely.
    #[test]
    fn v1_keyring_json_still_deserialises() {
        let v1 = r#"{
            "name": "mycloud",
            "url": "https://cloud.example.com/remote.php/dav/files/bob/",
            "username": "bob",
            "mount_point": "/home/bob/Cloud"
        }"#;
        let cfg: MountConfig = serde_json::from_str(v1).expect("v1 blob must still parse");

        assert_eq!(cfg.name, "mycloud");
        assert_eq!(cfg.username, "bob");
        assert_eq!(cfg.backend, Backend::WebDav);
        // ...and the new fields take their defaults.
        assert!(cfg.read_only);
        assert_eq!(cfg.metadata_ttl_secs, 30);
        assert_eq!(cfg.on_open_uncached, OnOpenUncached::Block);
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        // A newer version must be able to add fields an older one ignores.
        let json = r#"{
            "name": "m", "url": "https://x/", "username": "u",
            "mount_point": "/m", "something_from_the_future": 42
        }"#;
        assert!(serde_json::from_str::<MountConfig>(json).is_ok());
    }

    #[test]
    fn config_round_trips_through_json() {
        let original = sample();
        let json = serde_json::to_string(&original).unwrap();
        let back: MountConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, original.name);
        assert_eq!(back.backend, original.backend);
        assert_eq!(back.read_only, original.read_only);
    }

    /// The property that keeps `mount` and `daemon` behaving identically.
    #[test]
    fn env_round_trip_reproduces_the_config() {
        let mut original = sample();
        original.read_only = false;
        original.allow_other = true;
        original.max_concurrent = 9;
        original.on_open_uncached = OnOpenUncached::Background;
        original.metadata_ttl_secs = 77;

        let mut env: std::collections::HashMap<String, String> =
            original.to_env().into_iter().collect();
        env.insert("DAVFS_NAME".into(), original.name.clone());
        let env = MapEnv(env);

        let rebuilt = MountConfig::from_env(&env).expect("from_env");

        assert_eq!(rebuilt.url, original.url);
        assert_eq!(rebuilt.username, original.username);
        assert_eq!(rebuilt.mount_point, original.mount_point);
        assert_eq!(rebuilt.read_only, original.read_only);
        assert_eq!(rebuilt.allow_other, original.allow_other);
        assert_eq!(rebuilt.max_concurrent, original.max_concurrent);
        assert_eq!(rebuilt.on_open_uncached, original.on_open_uncached);
        assert_eq!(rebuilt.metadata_ttl_secs, original.metadata_ttl_secs);
    }

    /// The same round trip through a rendered systemd drop-in, which is the
    /// path `davfs-sync mount` and `davfs-sync daemon` actually differ on.
    #[test]
    fn a_dropin_reproduces_the_same_config() {
        let mut original = sample();
        original.mount_point = "/home/alice/My Cloud 100%".into();

        let rendered = crate::dropin::render(&original.to_env());
        let parsed = crate::dropin::parse(&rendered);

        let mut env: std::collections::HashMap<String, String> = parsed.into_iter().collect();
        env.insert("DAVFS_NAME".into(), original.name.clone());
        let rebuilt = MountConfig::from_env(&MapEnv(env)).expect("from_env");

        assert_eq!(
            rebuilt.mount_point, original.mount_point,
            "a mount point with a space and a percent did not survive the drop-in"
        );
        assert_eq!(rebuilt.url, original.url);
    }

    #[test]
    fn from_env_requires_the_essentials() {
        assert!(MountConfig::from_env(&MapEnv::default()).is_err());
        assert!(MountConfig::from_env(&MapEnv::from([("DAVFS_NAME", "x")])).is_err());
        assert!(MountConfig::from_env(&MapEnv::from([
            ("DAVFS_NAME", "x"),
            ("DAVFS_URL", "https://x/"),
        ]))
        .is_err());
        assert!(MountConfig::from_env(&MapEnv::from([
            ("DAVFS_NAME", "x"),
            ("DAVFS_URL", "https://x/"),
            ("DAVFS_MOUNT_POINT", "/m"),
        ]))
        .is_ok());
    }

    #[test]
    fn booleans_accept_the_spellings_people_use() {
        for (text, want) in [
            ("1", true),
            ("true", true),
            ("YES", true),
            ("on", true),
            ("0", false),
            ("false", false),
            ("no", false),
            ("Off", false),
        ] {
            assert_eq!(parse_bool(text), Some(want), "{text:?}");
        }
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn read_only_defaults_to_true() {
        // Flipping this is what enables writes later, so it must not drift.
        assert!(sample().read_only);
        assert!(sample().vfs_config().read_only);
    }

    #[test]
    fn the_backend_tag_is_stable() {
        // Stored in the database and compared on open; changing it silently
        // wipes every existing cache.
        assert_eq!(Backend::WebDav.tag(), "webdav");
        assert_eq!(
            Backend::GoogleDrive {
                root_folder_id: None,
                export_formats: vec![]
            }
            .tag(),
            "gdrive"
        );
    }

    #[test]
    fn cache_sizes_convert_to_bytes() {
        let mut cfg = sample();
        cfg.cache_max_gb = 2.0;
        cfg.cache_min_free_gb = 0.5;
        let sync = cfg.sync_config();
        assert_eq!(sync.cache_max_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(sync.cache_min_free_bytes, 512 * 1024 * 1024);
    }
}
