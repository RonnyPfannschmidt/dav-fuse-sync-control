# davfs-sync: WebDAV Virtual Filesystem with Offline Sync

## Overview

A FUSE-based WebDAV client for Linux providing on-demand file access with offline support, selective sync, and progress signaling.

**Design Philosophy:** Configuration lives in systemd unit drop-ins. Secrets use `systemd-creds`. The daemon reads configuration from environment variables set by systemd.

---

## Implementation Stages

### Stage 1: Core FUSE Mount (MVP)
- [ ] Basic FUSE operations (getattr, readdir, open, read)
- [ ] WebDAV client wrapper (webdav4)
- [ ] Simple file caching (download on open)
- [ ] SQLite metadata store
- [ ] Environment-based configuration
- [ ] CLI: `davfs-sync daemon` (runs under systemd)

### Stage 2: Sync State & Control
- [ ] Sync state tracking (remote/local/syncing/error)
- [ ] Read-only xattrs (status, progress, sizes)
- [ ] Action xattrs (do_pin, do_download, do_free)
- [ ] Background sync worker with queue
- [ ] CLI: `davfs-ctl status/pin/free/download`

### Stage 3: Systemd Integration
- [ ] Template unit `davfs-sync@.service`
- [ ] Drop-in generator for named mounts
- [ ] Systemd credentials for passwords
- [ ] CLI: `davfs-sync setup <name>` creates drop-ins
- [ ] Watchdog integration

### Stage 4: Glob Rules & Defaults
- [ ] Glob patterns for pin/ignore/no-cache
- [ ] Per-mount rules in drop-in config
- [ ] CLI: `davfs-ctl check-rules`

### Stage 5: File Manager Plugins
- [ ] Nautilus extension
- [ ] Dolphin service menus
- [ ] Nemo actions
- [ ] Status emblems

### Stage 6: Polish & Advanced Features
- [ ] Conflict detection and resolution
- [ ] Bandwidth limiting
- [ ] D-Bus interface
- [ ] Tray icon

---

## Project Structure

```
davfs-sync/
├── pyproject.toml
├── README.md
├── src/
│   └── davfs_sync/
│       ├── __init__.py
│       ├── cli.py                  # CLI entry (davfs-sync)
│       ├── ctl.py                  # Control tool (davfs-ctl)
│       ├── config.py               # Environment-based config
│       ├── fuse_ops.py             # FUSE implementation
│       ├── cache.py                # Local cache management
│       ├── metadata.py             # SQLite store
│       ├── sync_state.py           # State tracking
│       ├── sync_worker.py          # Background sync
│       ├── webdav.py               # WebDAV client wrapper
│       ├── xattr_handler.py        # Extended attributes
│       ├── glob_rules.py           # Pattern matching
│       └── systemd/
│           ├── __init__.py
│           ├── setup.py            # Drop-in generator
│           └── credentials.py      # systemd-creds wrapper
├── systemd/
│   ├── davfs-sync@.service         # Template unit
│   └── davfs-sync.target           # Grouping target
├── file-managers/
│   ├── nautilus/
│   ├── dolphin/
│   └── nemo/
└── tests/
```

---

## Systemd Configuration Architecture

### Directory Layout

```
~/.config/systemd/user/
├── davfs-sync@.service                         # Template (installed by package)
├── davfs-sync.target                           # Groups all mounts
│
├── davfs-sync@personal.service.d/              # "personal" mount config
│   └── mount.conf                              # All settings for this mount
│
├── davfs-sync@work.service.d/                  # "work" mount config  
│   └── mount.conf
│
└── davfs-sync@media.service.d/                 # "media" mount config
    └── mount.conf

~/.config/davfs-sync/
└── credentials/                                # Encrypted credentials
    ├── personal.cred
    ├── work.cred
    └── media.cred

~/.local/share/davfs-sync/
├── personal/
│   └── metadata.db
├── work/
│   └── metadata.db
└── media/
    └── metadata.db

~/.cache/davfs-sync/
├── personal/                                   # File cache
├── work/
└── media/
```

### Template Unit

```ini
# /usr/lib/systemd/user/davfs-sync@.service
# Template unit - instance name (%i) is the mount name
#
# Configure via drop-in: ~/.config/systemd/user/davfs-sync@<name>.service.d/mount.conf

[Unit]
Description=DavFS Sync: %i
Documentation=man:davfs-sync(1)
After=network-online.target
Wants=network-online.target
PartOf=davfs-sync.target

# Only start if configured
ConditionPathExists=%E/systemd/user/davfs-sync@%i.service.d/mount.conf

[Service]
Type=notify
NotifyAccess=main

# Mount name passed via environment
Environment=DAVFS_NAME=%i

# Daemon reads all config from environment (set in drop-in)
ExecStart=/usr/bin/davfs-sync daemon
ExecReload=/bin/kill -HUP $MAINPID

# Graceful unmount
ExecStop=/usr/bin/davfs-sync unmount --wait
TimeoutStopSec=30

Restart=on-failure
RestartSec=10

# Watchdog
WatchdogSec=60

# Load encrypted credential
LoadCredentialEncrypted=password:%E/davfs-sync/credentials/%i.cred

# Security
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=read-only
# ReadWritePaths set in drop-in (depends on mount point)
PrivateTmp=yes

# Logging
StandardOutput=journal
StandardError=journal
SyslogIdentifier=davfs-sync@%i

[Install]
WantedBy=davfs-sync.target
```

### Drop-in Configuration

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

### Target Unit

```ini
# /usr/lib/systemd/user/davfs-sync.target

[Unit]
Description=DavFS Sync Mounts
Documentation=man:davfs-sync(1)
After=network-online.target

[Install]
WantedBy=default.target
```

---

## Credential Management

### Create Credential

```bash
# Interactive (password not echoed)
$ davfs-sync setup personal
...
Password: ********
Encrypting credential...

# Or manually with systemd-creds
$ systemd-ask-password "Password:" | \
    systemd-creds encrypt --name=password - \
    ~/.config/davfs-sync/credentials/personal.cred
```

### Update Password

```bash
$ davfs-sync set-password personal
Password: ********
Credential updated. Restart service to apply:
  systemctl --user restart davfs-sync@personal
```

### How It Works

1. `systemd-creds encrypt` creates encrypted credential bound to local TPM/machine
2. Template unit has `LoadCredentialEncrypted=password:%E/davfs-sync/credentials/%i.cred`
3. At service start, systemd decrypts to `$CREDENTIALS_DIRECTORY/password`
4. Daemon reads password from that file

### Credential Helper

```python
# davfs_sync/systemd/credentials.py

import os
import subprocess
from pathlib import Path
from getpass import getpass

class CredentialManager:
    def __init__(self):
        self.cred_dir = Path.home() / ".config/davfs-sync/credentials"
        self.cred_dir.mkdir(parents=True, exist_ok=True)
    
    def path(self, name: str) -> Path:
        return self.cred_dir / f"{name}.cred"
    
    def exists(self, name: str) -> bool:
        return self.path(name).exists()
    
    def encrypt(self, name: str, password: str):
        """Encrypt password to credential file."""
        result = subprocess.run(
            ["systemd-creds", "encrypt", "--name=password", "-", str(self.path(name))],
            input=password.encode(),
            capture_output=True
        )
        if result.returncode != 0:
            raise RuntimeError(f"systemd-creds failed: {result.stderr.decode()}")
        self.path(name).chmod(0o600)
    
    def encrypt_interactive(self, name: str):
        """Prompt for password and encrypt."""
        pw = getpass(f"Password for {name}: ")
        self.encrypt(name, pw)
    
    def read_runtime(self) -> str:
        """Read decrypted credential at runtime (under systemd)."""
        creds_dir = os.environ.get("CREDENTIALS_DIRECTORY")
        if not creds_dir:
            raise RuntimeError("Not running under systemd with credentials")
        return (Path(creds_dir) / "password").read_text()
    
    def delete(self, name: str):
        p = self.path(name)
        if p.exists():
            p.unlink()
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

## Config Loader

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

---

## CLI: `davfs-sync`

### Setup Commands

```bash
# Interactive setup - creates drop-in + credential
$ davfs-sync setup personal
WebDAV URL: https://cloud.example.com/remote.php/dav/files/user/
Username: myuser
Password: ********
Mount point [~/Cloud/Personal]: 

Creating: ~/.config/systemd/user/davfs-sync@personal.service.d/mount.conf
Creating: ~/.config/davfs-sync/credentials/personal.cred
Creating: ~/.cache/davfs-sync/personal/
Creating: ~/.local/share/davfs-sync/personal/
Creating: ~/Cloud/Personal/

Done! Commands:
  systemctl --user daemon-reload
  systemctl --user start davfs-sync@personal
  systemctl --user enable davfs-sync@personal   # start at login

# Non-interactive
$ davfs-sync setup work \
    --url "https://work.com/dav/" \
    --username "me@work.com" \
    --mount "~/Cloud/Work" \
    --password-stdin < /path/to/pw

# With options
$ davfs-sync setup media \
    --url "https://nas/dav/" \
    --username "user" \
    --mount "~/Media" \
    --cache-max-gb 50 \
    --on-open-uncached block \
    --rule-pin "Favorites/**" \
    --rule-no-cache "**"
```

### Management Commands

```bash
# List configured mounts
$ davfs-sync list
NAME      URL                              MOUNT              STATUS   ENABLED
personal  https://cloud.example.com/...    ~/Cloud/Personal   active   yes
work      https://work.com/dav/            ~/Cloud/Work       active   yes  
media     https://nas/dav/                 ~/Media            inactive no

# Show configuration
$ davfs-sync show personal
Name: personal
URL: https://cloud.example.com/remote.php/dav/files/user/
Username: myuser
Mount: /home/user/Cloud/Personal
Cache: 20 GB max, evict after 30 days
Rules:
  pin: Documents/Important/**
  ignore: *.tmp; Trash/**

# Edit drop-in (opens $EDITOR)
$ davfs-sync edit personal

# Update password
$ davfs-sync set-password personal

# Remove mount
$ davfs-sync remove personal
This will:
  - Stop the service
  - Remove drop-in configuration
  - Remove credential
  - Keep cache and metadata (use --purge to delete)
Proceed? [y/N]: y
```

### Service Control (wraps systemctl)

```bash
$ davfs-sync start personal
$ davfs-sync stop personal
$ davfs-sync restart personal
$ davfs-sync enable personal      # start at login
$ davfs-sync disable personal
$ davfs-sync status               # all mounts
$ davfs-sync status personal      # specific mount
$ davfs-sync logs personal        # journalctl wrapper
$ davfs-sync logs personal -f     # follow
```

### Rule Management

```bash
# Add rules
$ davfs-sync rule personal --add-pin "Projects/**"
$ davfs-sync rule personal --add-ignore "*.log"
$ davfs-sync rule personal --add-no-cache "Videos/**"

# Remove rules  
$ davfs-sync rule personal --remove-pin "Projects/**"

# List rules
$ davfs-sync rule personal --list
pin:
  - Documents/Important/**
  - Projects/**
ignore:
  - *.tmp
  - *.log
no_cache:
  - Videos/**
max_size_mb: 100
```

---

## CLI: `davfs-ctl`

```bash
# Status (auto-detects mount from path)
$ davfs-ctl status ~/Cloud/Personal/Documents/
$ davfs-ctl status -r ~/Cloud/Personal/         # recursive

# Pin/unpin
$ davfs-ctl pin ~/Cloud/file.pdf
$ davfs-ctl pin -r ~/Cloud/Projects/
$ davfs-ctl unpin ~/Cloud/old/

# Download (without pinning)
$ davfs-ctl download ~/Cloud/file.pdf
$ davfs-ctl download -r ~/Cloud/Archive/

# Free cache
$ davfs-ctl free ~/Cloud/big.iso
$ davfs-ctl free -r ~/Cloud/old-backups/

# Cancel transfer
$ davfs-ctl cancel ~/Cloud/file.pdf

# Watch active transfers
$ davfs-ctl watch
$ davfs-ctl watch --mount personal

# Queue info
$ davfs-ctl queue
$ davfs-ctl queue --mount personal

# Cache usage
$ davfs-ctl cache
$ davfs-ctl cache --mount personal

# Find by status
$ davfs-ctl find --local ~/Cloud/
$ davfs-ctl find --remote ~/Cloud/
$ davfs-ctl find --pinned ~/Cloud/

# Test glob rules
$ davfs-ctl check-rules ~/Cloud/some/file.pdf
Path: some/file.pdf
Matched: pin
Pattern: **/*.pdf
Action: Will be auto-pinned

# Conflicts
$ davfs-ctl conflicts
$ davfs-ctl resolve --keep-local ~/Cloud/file.pdf
$ davfs-ctl resolve --keep-remote ~/Cloud/file.pdf
```

---

## Extended Attributes

### Read-Only Status

| Attribute | Description |
|-----------|-------------|
| `user.davfs.status` | `remote`/`syncing`/`local`/`modified`/`conflict`/`error` |
| `user.davfs.pinned` | `1` or `0` |
| `user.davfs.progress` | `0.00` to `1.00` (when syncing) |
| `user.davfs.progress_bytes` | `downloaded/total` |
| `user.davfs.remote_size` | Server size in bytes |
| `user.davfs.local_size` | Cached size in bytes |
| `user.davfs.etag` | Server ETag |
| `user.davfs.error` | Error message (when status=error) |
| `user.davfs.rule_match` | Matched glob pattern |

### Directory Aggregates (Read-Only)

| Attribute | Description |
|-----------|-------------|
| `user.davfs.sync_active` | Count of active syncs under dir |
| `user.davfs.sync_pending` | Count of pending syncs |
| `user.davfs.sync_files` | JSON array of syncing paths |
| `user.davfs.sync_bytes_total` | Total bytes being synced |
| `user.davfs.sync_bytes_done` | Completed bytes |
| `user.davfs.children_local` | Local children count |
| `user.davfs.children_remote` | Remote children count |

### Action Triggers (Write-Only)

Write any value to trigger. Reading returns `ENODATA`.

| Attribute | Description |
|-----------|-------------|
| `user.davfs.do_pin` | Pin file/dir |
| `user.davfs.do_pin_r` | Pin recursively |
| `user.davfs.do_unpin` | Unpin |
| `user.davfs.do_unpin_r` | Unpin recursively |
| `user.davfs.do_download` | Download now |
| `user.davfs.do_download_r` | Download recursively |
| `user.davfs.do_free` | Free cache |
| `user.davfs.do_free_r` | Free recursively |
| `user.davfs.do_refresh` | Refresh metadata |
| `user.davfs.do_refresh_r` | Refresh recursively |
| `user.davfs.do_upload` | Force upload |
| `user.davfs.do_cancel` | Cancel transfer |

### Usage

```bash
# Check status
$ getfattr -n user.davfs.status ~/Cloud/file.pdf
user.davfs.status="remote"

# Trigger download
$ setfattr -n user.davfs.do_download -v 1 ~/Cloud/file.pdf

# Check progress
$ getfattr -n user.davfs.progress ~/Cloud/file.pdf
user.davfs.progress="0.45"

# See what's syncing in a folder
$ getfattr -n user.davfs.sync_files ~/Cloud/Projects/
user.davfs.sync_files="[\"Projects/a.zip\",\"Projects/b.tar\"]"

# List all davfs attrs
$ getfattr -d -m "user.davfs.*" ~/Cloud/file.pdf
```

---

## File Manager Plugins

### Nautilus (Python Extension)

```python
# ~/.local/share/nautilus-python/extensions/davfs-sync.py

from gi.repository import Nautilus, GObject
import os

class DavFSSyncExtension(GObject.GObject, Nautilus.MenuProvider):
    def _is_davfs(self, path):
        try:
            os.getxattr(path, "user.davfs.status")
            return True
        except:
            return False
    
    def get_file_items(self, files):
        if len(files) != 1:
            return []
        
        path = files[0].get_location().get_path()
        if not self._is_davfs(path):
            return []
        
        status = os.getxattr(path, "user.davfs.status").decode()
        is_dir = files[0].is_directory()
        
        items = []
        
        if status == "remote":
            item = Nautilus.MenuItem(
                name="DavFS::Download",
                label="Download from Cloud",
                icon="cloud-download"
            )
            attr = "user.davfs.do_download_r" if is_dir else "user.davfs.do_download"
            item.connect("activate", lambda *_: os.setxattr(path, attr, b"1"))
            items.append(item)
        
        # ... more menu items
        
        return items
```

### Dolphin (Service Menu)

```ini
# ~/.local/share/kio/servicemenus/davfs-sync.desktop

[Desktop Entry]
Type=Service
MimeType=all/allfiles;inode/directory;
Actions=Download;Pin;Free;

[Desktop Action Download]
Name=Download from Cloud
Icon=cloud-download
Exec=setfattr -n user.davfs.do_download -v 1 %f

[Desktop Action Pin]
Name=Keep Offline
Icon=folder-favorites
Exec=setfattr -n user.davfs.do_pin -v 1 %f

[Desktop Action Free]
Name=Free Space
Icon=edit-clear
Exec=setfattr -n user.davfs.do_free -v 1 %f
```

---

## Systemd Version Compatibility

| Feature | Min Version | Fallback |
|---------|-------------|----------|
| User units | 206 | N/A |
| `LoadCredentialEncrypted` | 250 | Plain file |
| `systemd-creds` | 250 | Manual encryption |

### Fallback for Older Systems

```ini
# Use plain credential file instead of encrypted
# ~/.config/systemd/user/davfs-sync@personal.service.d/mount.conf

[Service]
# Instead of LoadCredentialEncrypted in template
LoadCredential=password:%h/.config/davfs-sync/credentials/personal.txt
```

Setup command detects systemd version and uses appropriate method.

---

## Dependencies

```toml
[project]
name = "davfs-sync"
version = "0.1.0"
requires-python = ">=3.10"
dependencies = [
    "fusepy>=3.0.1",
    "webdav4>=0.10.0",
    "httpx>=0.27.0",
    "click>=8.0",
    "rich>=13.0",
]

[project.scripts]
davfs-sync = "davfs_sync.cli:main"
davfs-ctl = "davfs_sync.ctl:main"
```
