# davfs-sync: Configuration

## Drop-in Configuration

Each mount has a single `mount.conf` drop-in with all settings:

```ini
# ~/.config/systemd/user/davfs-sync@personal.service.d/mount.conf
#
# Configuration for: personal
# Created by: davfs-sync setup personal

[Service]
# === Connection (required) ===
Environment=DAVFS_URL=https://cloud.example.com/remote.php/dav/files/user/
Environment=DAVFS_USERNAME=myuser
Environment=DAVFS_MOUNT_POINT=%h/Cloud/Personal

# === Paths ===
# Defaults use mount name, override if needed
#Environment=DAVFS_CACHE_DIR=%h/.cache/davfs-sync/personal
#Environment=DAVFS_DATA_DIR=%h/.local/share/davfs-sync/personal

# === Cache ===
Environment=DAVFS_CACHE_MAX_GB=20
Environment=DAVFS_CACHE_EVICT_DAYS=30
Environment=DAVFS_CACHE_MIN_FREE_GB=5

# === Behavior ===
# on_open_uncached: error | block | background
Environment=DAVFS_ON_OPEN_UNCACHED=error
Environment=DAVFS_METADATA_REFRESH_MIN=15

# === Network ===
Environment=DAVFS_MAX_CONCURRENT=3
Environment=DAVFS_TIMEOUT_SEC=30
Environment=DAVFS_RETRY_COUNT=3

# === Glob Rules (semicolon-separated patterns) ===
# Pin: always keep offline
Environment=DAVFS_RULE_PIN=Documents/Important/**;*.keepoffline

# Ignore: don't sync at all
Environment=DAVFS_RULE_IGNORE=*.tmp;~*;.~lock.*;Trash/**

# No-cache: never auto-cache (stream only)
Environment=DAVFS_RULE_NO_CACHE=Archive/**;*.iso;*.vmdk

# Max size for auto-caching (MB)
Environment=DAVFS_RULE_MAX_SIZE_MB=100

# === Logging ===
Environment=DAVFS_LOG_LEVEL=INFO

# === Security: allow writes to mount and cache ===
ReadWritePaths=%h/Cloud/Personal
ReadWritePaths=%h/.cache/davfs-sync/personal
ReadWritePaths=%h/.local/share/davfs-sync/personal
```

---

## Environment Variables

### Required

| Variable | Description |
|----------|-------------|
| `DAVFS_NAME` | Mount name (set by template from %i) |
| `DAVFS_URL` | WebDAV server URL |
| `DAVFS_USERNAME` | Authentication username |
| `DAVFS_MOUNT_POINT` | Local mount path |

### Optional

| Variable | Default | Description |
|----------|---------|-------------|
| `DAVFS_CACHE_DIR` | `~/.cache/davfs-sync/$name` | Cache location |
| `DAVFS_DATA_DIR` | `~/.local/share/davfs-sync/$name` | Metadata location |
| `DAVFS_CACHE_MAX_GB` | `10` | Maximum cache size |
| `DAVFS_CACHE_EVICT_DAYS` | `30` | Days before LRU eviction |
| `DAVFS_CACHE_MIN_FREE_GB` | `5` | Minimum free disk space |
| `DAVFS_ON_OPEN_UNCACHED` | `error` | `error`/`block`/`background` |
| `DAVFS_METADATA_REFRESH_MIN` | `15` | Metadata refresh interval |
| `DAVFS_MAX_CONCURRENT` | `3` | Concurrent transfers |
| `DAVFS_TIMEOUT_SEC` | `30` | Network timeout |
| `DAVFS_RETRY_COUNT` | `3` | Retry attempts |
| `DAVFS_LOG_LEVEL` | `INFO` | Log verbosity |

### Glob Rules

| Variable | Default | Description |
|----------|---------|-------------|
| `DAVFS_RULE_PIN` | (empty) | Patterns to auto-pin |
| `DAVFS_RULE_IGNORE` | (empty) | Patterns to ignore |
| `DAVFS_RULE_NO_CACHE` | (empty) | Patterns to never cache |
| `DAVFS_RULE_MAX_SIZE_MB` | `100` | Max auto-cache file size |

Patterns are semicolon-separated: `*.tmp;Trash/**;.git/**`

---

## Configuration Loader Implementation

### Rules Struct

```rust
// src/config/mod.rs

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rules {
    pub pin: Vec<String>,
    pub ignore: Vec<String>,
    pub no_cache: Vec<String>,
    pub max_size_mb: u64,
}

impl Rules {
    pub fn from_env() -> Self {
        fn parse_semicolon(key: &str) -> Vec<String> {
            std::env::var(key)
                .unwrap_or_default()
                .split(';')
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_string())
                .collect()
        }
        
        Self {
            pin: parse_semicolon("DAVFS_RULE_PIN"),
            ignore: parse_semicolon("DAVFS_RULE_IGNORE"),
            no_cache: parse_semicolon("DAVFS_RULE_NO_CACHE"),
            max_size_mb: std::env::var("DAVFS_RULE_MAX_SIZE_MB")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(100),
        }
    }
}
```

### Config Struct

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub name: String,
    pub url: String,
    pub username: String,
    pub password: String,
    pub mount_point: PathBuf,
    pub cache_dir: PathBuf,
    pub data_dir: PathBuf,
    
    pub cache_max_gb: f64,
    pub cache_evict_days: i32,
    pub cache_min_free_gb: f64,
    
    pub on_open_uncached: String,
    pub metadata_refresh_min: i32,
    
    pub max_concurrent: usize,
    pub timeout_sec: u64,
    pub retry_count: u32,
    
    pub log_level: String,
    pub rules: Rules,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        use crate::secrets::{systemd_creds, secret_service};
        
        let name = std::env::var("DAVFS_NAME")?;
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!(\"No home dir\"))?;
        
        // Load password: try systemd credential, then Secret Service, then env
        let password = systemd_creds::read_password()
            .or_else(|_| secret_service::get_password(&name))
            .or_else(|_| std::env::var("DAVFS_PASSWORD")
                .map_err(|_| anyhow::anyhow!(\"No password available\")))?;
        
        Ok(Self {
            name: name.clone(),
            url: std::env::var("DAVFS_URL")?,
            username: std::env::var("DAVFS_USERNAME")?,
            password,
            mount_point: PathBuf::from(std::env::var("DAVFS_MOUNT_POINT")?),
            cache_dir: std::env::var("DAVFS_CACHE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home.join(\".cache/davfs-sync\").join(&name)),
            data_dir: std::env::var("DAVFS_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home.join(\".local/share/davfs-sync\").join(&name)),
            cache_max_gb: std::env::var(\"DAVFS_CACHE_MAX_GB\")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(10.0),
            cache_evict_days: std::env::var(\"DAVFS_CACHE_EVICT_DAYS\")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(30),
            cache_min_free_gb: std::env::var(\"DAVFS_CACHE_MIN_FREE_GB\")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(5.0),
            on_open_uncached: std::env::var(\"DAVFS_ON_OPEN_UNCACHED\")
                .unwrap_or_else(|_| \"error\".to_string()),
            metadata_refresh_min: std::env::var(\"DAVFS_METADATA_REFRESH_MIN\")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(15),
            max_concurrent: std::env::var(\"DAVFS_MAX_CONCURRENT\")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(3),
            timeout_sec: std::env::var(\"DAVFS_TIMEOUT_SEC\")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(30),
            retry_count: std::env::var(\"DAVFS_RETRY_COUNT\")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(3),
            log_level: std::env::var(\"DAVFS_LOG_LEVEL\")
                .unwrap_or_else(|_| \"INFO\".to_string()),
            rules: Rules::from_env(),
        })
    }
    
    pub fn metadata_db(&self) -> PathBuf {
        self.data_dir.join(\"metadata.db\")
    }
}
```
