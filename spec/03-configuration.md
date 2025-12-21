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

### Rules Dataclass

```python
# davfs_sync/config.py

import os
from pathlib import Path
from dataclasses import dataclass, field
from .systemd.credentials import CredentialManager

@dataclass
class Rules:
    pin: list[str] = field(default_factory=list)
    ignore: list[str] = field(default_factory=list)
    no_cache: list[str] = field(default_factory=list)
    max_size_mb: int = 100
    
    @classmethod
    def from_env(cls) -> "Rules":
        def parse(key: str) -> list[str]:
            val = os.environ.get(key, "")
            return [p.strip() for p in val.split(";") if p.strip()]
        
        return cls(
            pin=parse("DAVFS_RULE_PIN"),
            ignore=parse("DAVFS_RULE_IGNORE"),
            no_cache=parse("DAVFS_RULE_NO_CACHE"),
            max_size_mb=int(os.environ.get("DAVFS_RULE_MAX_SIZE_MB", "100")),
        )
```

### Config Dataclass

```python
@dataclass
class Config:
    name: str
    url: str
    username: str
    password: str
    mount_point: Path
    cache_dir: Path
    data_dir: Path
    
    cache_max_gb: float
    cache_evict_days: int
    cache_min_free_gb: float
    
    on_open_uncached: str
    metadata_refresh_min: int
    
    max_concurrent: int
    timeout_sec: int
    retry_count: int
    
    log_level: str
    rules: Rules
    
    @classmethod
    def from_env(cls) -> "Config":
        name = os.environ["DAVFS_NAME"]
        home = Path.home()
        
        # Load password from systemd credential or fallback
        try:
            password = CredentialManager().read_runtime()
        except RuntimeError:
            password = os.environ.get("DAVFS_PASSWORD", "")
            if not password:
                raise ValueError("No password available")
        
        return cls(
            name=name,
            url=os.environ["DAVFS_URL"],
            username=os.environ["DAVFS_USERNAME"],
            password=password,
            mount_point=Path(os.environ["DAVFS_MOUNT_POINT"]).expanduser(),
            cache_dir=Path(os.environ.get(
                "DAVFS_CACHE_DIR", home / ".cache/davfs-sync" / name
            )),
            data_dir=Path(os.environ.get(
                "DAVFS_DATA_DIR", home / ".local/share/davfs-sync" / name
            )),
            cache_max_gb=float(os.environ.get("DAVFS_CACHE_MAX_GB", "10")),
            cache_evict_days=int(os.environ.get("DAVFS_CACHE_EVICT_DAYS", "30")),
            cache_min_free_gb=float(os.environ.get("DAVFS_CACHE_MIN_FREE_GB", "5")),
            on_open_uncached=os.environ.get("DAVFS_ON_OPEN_UNCACHED", "error"),
            metadata_refresh_min=int(os.environ.get("DAVFS_METADATA_REFRESH_MIN", "15")),
            max_concurrent=int(os.environ.get("DAVFS_MAX_CONCURRENT", "3")),
            timeout_sec=int(os.environ.get("DAVFS_TIMEOUT_SEC", "30")),
            retry_count=int(os.environ.get("DAVFS_RETRY_COUNT", "3")),
            log_level=os.environ.get("DAVFS_LOG_LEVEL", "INFO"),
            rules=Rules.from_env(),
        )
    
    @property
    def metadata_db(self) -> Path:
        return self.data_dir / "metadata.db"
```
