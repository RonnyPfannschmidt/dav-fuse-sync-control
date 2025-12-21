# davfs-sync: Architecture

## Project Structure

```
davfs-sync/
├── pyproject.toml
├── README.md
├── src/
│   └── davfs_sync/
│       ├── __init__.py
│       ├── cli.py                  # CLI entry (all commands)
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

---

## Template Unit

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

---

## Target Unit

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

### How It Works

1. `systemd-creds encrypt` creates encrypted credential bound to local TPM/machine
2. Template unit has `LoadCredentialEncrypted=password:%E/davfs-sync/credentials/%i.cred`
3. At service start, systemd decrypts to `$CREDENTIALS_DIRECTORY/password`
4. Daemon reads password from that file

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

### Credential Helper Implementation

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
