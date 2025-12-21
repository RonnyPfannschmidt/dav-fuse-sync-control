# davfs-sync: Overview and Implementation Plan

## Project Overview

**davfs-sync** is a FUSE-based WebDAV client for Linux providing on-demand file access with offline support, selective sync, and progress signaling.

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
- [ ] CLI: `davfs-sync status/pin/free/download`

### Stage 3: Systemd Integration
- [ ] Template unit `davfs-sync@.service`
- [ ] Drop-in generator for named mounts
- [ ] Systemd credentials for passwords
- [ ] CLI: `davfs-sync setup <name>` creates drop-ins
- [ ] Watchdog integration

### Stage 4: Glob Rules & Defaults
- [ ] Glob patterns for pin/ignore/no-cache
- [ ] Per-mount rules in drop-in config
- [ ] CLI: `davfs-sync check-rules`

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
```

---

## Next Steps

1. See [02-architecture.md](02-architecture.md) for system architecture and directory structure
2. See [03-configuration.md](03-configuration.md) for configuration details
3. See [04-cli.md](04-cli.md) for command-line interface specification
4. See [05-integration.md](05-integration.md) for file manager integration
