# davfs-sync: Architecture

## Project Structure

```
davfs-sync/
├── Cargo.toml
├── Cargo.lock
├── README.md
├── src/
│   ├── main.rs                     # CLI entry point
│   ├── cli/
│   │   ├── mod.rs
│   │   ├── setup.rs                # Setup commands
│   │   ├── service.rs              # Service control
│   │   └── file_ops.rs             # File operations
│   ├── config/
│   │   ├── mod.rs
│   │   ├── env.rs                  # Environment-based config
│   │   ├── storage.rs              # Config storage abstraction
│   │   ├── systemd.rs              # Systemd drop-ins
│   │   └── gsettings.rs            # GSettings integration
│   ├── fuse/
│   │   ├── mod.rs
│   │   ├── filesystem.rs           # FUSE implementation
│   │   └── operations.rs           # FUSE operations
│   ├── cache/
│   │   ├── mod.rs
│   │   └── manager.rs              # Cache management
│   ├── metadata/
│   │   ├── mod.rs
│   │   └── store.rs                # SQLite store
│   ├── sync/
│   │   ├── mod.rs
│   │   ├── state.rs                # State tracking
│   │   └── worker.rs               # Background sync
│   ├── webdav/
│   │   ├── mod.rs
│   │   └── client.rs               # WebDAV client
│   ├── xattr/
│   │   ├── mod.rs
│   │   └── handler.rs              # Extended attributes
│   ├── rules/
│   │   ├── mod.rs
│   │   └── glob.rs                 # Pattern matching
│   └── secrets/
│       ├── mod.rs
│       ├── systemd_creds.rs        # systemd-creds wrapper
│       └── secret_service.rs       # Secret Service API
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

## Configuration Storage Options

The project supports multiple configuration storage backends:

1. **Systemd drop-ins** (traditional, stateless)
2. **GSettings/dconf** (GNOME integration)
3. **Secret Service API** (GNOME Keyring, KWallet)

### Storage Comparison

| Feature | Systemd Drop-ins | GSettings | Secret Service |
|---------|------------------|-----------|----------------|
| Desktop integration | ❌ | ✅ GNOME | ✅ Cross-desktop |
| GUI configuration | ❌ | ✅ | ✅ |
| Password storage | systemd-creds | Secret Service | ✅ |
| Portability | ✅ | GNOME only | ✅ |
| User-friendly | ❌ | ✅ | ✅ |
| Server-friendly | ✅ | ❌ | ❌ |

### Implementation Strategy

```rust
// src/config/storage.rs

pub trait ConfigStorage {
    fn save_mount(&self, name: &str, config: &MountConfig) -> Result<()>;
    fn load_mount(&self, name: &str) -> Result<MountConfig>;
    fn list_mounts(&self) -> Result<Vec<String>>;
    fn remove_mount(&self, name: &str) -> Result<()>;
}

pub enum StorageBackend {
    Systemd(SystemdStorage),
    GSettings(GSettingsStorage),
}

impl ConfigStorage for StorageBackend {
    fn save_mount(&self, name: &str, config: &MountConfig) -> Result<()> {
        match self {
            Self::Systemd(s) => s.save_mount(name, config),
            Self::GSettings(s) => s.save_mount(name, config),
        }
    }
    // ...
}
```

The daemon reads from environment (systemd) OR queries GSettings at startup.

---

## Credential Management

### Option 1: systemd-creds (Systemd Drop-ins)

#### How It Works

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

---

### Option 2: Secret Service API (GNOME Keyring, KWallet)

#### How It Works

1. Implements [Secret Service API](https://specifications.freedesktop.org/secret-service/) (D-Bus)
2. Supported by GNOME Keyring, KWallet, pass-secret-service, etc.
3. Passwords stored in desktop keyring, unlocked with user login
4. Cross-desktop compatible

#### Usage

```bash
# Store password in keyring
$ davfs-sync setup personal --storage gsettings
WebDAV URL: https://cloud.example.com/...
Username: myuser
Password: ********

# Password stored in Secret Service with attributes:
# - application: davfs-sync
# - mount: personal
# - username: myuser
```

#### Implementation

```rust
// src/secrets/secret_service.rs

use secret_service::{SecretService, EncryptionType};
use std::collections::HashMap;

pub struct SecretServiceStore {
    service: SecretService<'static>,
}

impl SecretServiceStore {
    pub fn new() -> Result<Self> {
        let service = SecretService::connect(EncryptionType::Dh)?;
        Ok(Self { service })
    }
    
    pub fn store_password(&self, mount: &str, username: &str, password: &str) -> Result<()> {
        let collection = self.service.get_default_collection()?;
        
        let mut attributes = HashMap::new();
        attributes.insert("application", "davfs-sync");
        attributes.insert("mount", mount);
        attributes.insert("username", username);
        
        collection.create_item(
            &format!("davfs-sync: {}", mount),
            attributes,
            password.as_bytes(),
            true,  // replace existing
            "text/plain",
        )?;
        
        Ok(())
    }
    
    pub fn get_password(&self, mount: &str) -> Result<String> {
        let collection = self.service.get_default_collection()?;
        
        let mut attributes = HashMap::new();
        attributes.insert("application", "davfs-sync");
        attributes.insert("mount", mount);
        
        let items = collection.search_items(attributes)?;
        let item = items.first().ok_or_else(|| anyhow!("Password not found"))?;
        let secret = item.get_secret()?;
        
        Ok(String::from_utf8(secret)?)
    }
    
    pub fn delete_password(&self, mount: &str) -> Result<()> {
        let collection = self.service.get_default_collection()?;
        
        let mut attributes = HashMap::new();
        attributes.insert("application", "davfs-sync");
        attributes.insert("mount", mount);
        
        let items = collection.search_items(attributes)?;
        for item in items {
            item.delete()?;
        }
        
        Ok(())
    }
}
```

---

### Option 3: GSettings for Configuration

#### Schema Definition

```xml
<!-- /usr/share/glib-2.0/schemas/com.github.davfs-sync.gschema.xml -->

<schemalist>
  <schema id="com.github.davfs-sync" path="/com/github/davfs-sync/">
    <child name="mounts" schema="com.github.davfs-sync.mounts"/>
  </schema>
  
  <schema id="com.github.davfs-sync.mounts" path="/com/github/davfs-sync/mounts/">
    <!-- Dynamic per-mount schemas created at runtime -->
  </schema>
  
  <schema id="com.github.davfs-sync.mount">
    <key name="url" type="s">
      <default>''</default>
      <summary>WebDAV server URL</summary>
    </key>
    
    <key name="username" type="s">
      <default>''</default>
      <summary>Authentication username</summary>
    </key>
    
    <key name="mount-point" type="s">
      <default>''</default>
      <summary>Local mount point path</summary>
    </key>
    
    <key name="cache-max-gb" type="d">
      <default>10.0</default>
      <summary>Maximum cache size in GB</summary>
    </key>
    
    <key name="cache-evict-days" type="i">
      <default>30</default>
      <summary>Days before LRU eviction</summary>
    </key>
    
    <key name="on-open-uncached" type="s">
      <choices>
        <choice value='error'/>
        <choice value='block'/>
        <choice value='background'/>
      </choices>
      <default>'error'</default>
      <summary>Behavior when opening uncached files</summary>
    </key>
    
    <key name="rule-pin" type="as">
      <default>[]</default>
      <summary>Glob patterns to auto-pin</summary>
    </key>
    
    <key name="rule-ignore" type="as">
      <default>[]</default>
      <summary>Glob patterns to ignore</summary>
    </key>
    
    <key name="rule-no-cache" type="as">
      <default>[]</default>
      <summary>Glob patterns to never cache</summary>
    </key>
  </schema>
</schemalist>
```

#### Implementation

```rust
// src/config/gsettings.rs

use gio::prelude::*;
use gio::Settings;

pub struct GSettingsStorage {
    base: Settings,
}

impl GSettingsStorage {
    pub fn new() -> Result<Self> {
        let base = Settings::new("com.github.davfs-sync");
        Ok(Self { base })
    }
    
    fn mount_settings(&self, name: &str) -> Settings {
        Settings::new_with_path(
            "com.github.davfs-sync.mount",
            &format!("/com/github/davfs-sync/mounts/{}/", name),
        )
    }
}

impl ConfigStorage for GSettingsStorage {
    fn save_mount(&self, name: &str, config: &MountConfig) -> Result<()> {
        let settings = self.mount_settings(name);
        
        settings.set_string("url", &config.url)?;
        settings.set_string("username", &config.username)?;
        settings.set_string("mount-point", config.mount_point.to_str().unwrap())?;
        settings.set_double("cache-max-gb", config.cache_max_gb)?;
        settings.set_int("cache-evict-days", config.cache_evict_days)?;
        settings.set_string("on-open-uncached", &config.on_open_uncached)?;
        
        settings.set_strv("rule-pin", &config.rules.pin)?;
        settings.set_strv("rule-ignore", &config.rules.ignore)?;
        settings.set_strv("rule-no-cache", &config.rules.no_cache)?;
        
        Ok(())
    }
    
    fn load_mount(&self, name: &str) -> Result<MountConfig> {
        let settings = self.mount_settings(name);
        
        Ok(MountConfig {
            url: settings.string("url").to_string(),
            username: settings.string("username").to_string(),
            mount_point: PathBuf::from(settings.string("mount-point").as_str()),
            cache_max_gb: settings.double("cache-max-gb"),
            cache_evict_days: settings.int("cache-evict-days"),
            on_open_uncached: settings.string("on-open-uncached").to_string(),
            rules: Rules {
                pin: settings.strv("rule-pin").iter().map(|s| s.to_string()).collect(),
                ignore: settings.strv("rule-ignore").iter().map(|s| s.to_string()).collect(),
                no_cache: settings.strv("rule-no-cache").iter().map(|s| s.to_string()).collect(),
                max_size_mb: settings.int("rule-max-size-mb"),
            },
        })
    }
    
    fn list_mounts(&self) -> Result<Vec<String>> {
        // List subdirectories under /com/github/davfs-sync/mounts/
        // Implementation depends on GSettings API
        unimplemented!()
    }
}
```

#### Benefits

- Desktop integration (dconf-editor GUI)
- Live config updates (monitor GSettings changes)
- Schema validation
- User-friendly for GNOME users
- Can still use systemd units, but config comes from GSettings

### Recommended Approach

**Default: Systemd drop-ins** for simplicity and server compatibility.

**Optional: GSettings + Secret Service** when:
- Running on GNOME/desktop environment
- User prefers GUI configuration
- Detected via `--storage=gsettings` flag

The daemon supports both: reads from environment (systemd) or queries GSettings/SecretService.
