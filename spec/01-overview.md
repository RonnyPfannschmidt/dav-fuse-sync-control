# davfs-sync: Overview and Implementation Plan

## Project Overview

**davfs-sync** is a FUSE-based WebDAV client for Linux providing on-demand file access with offline support, selective sync, and progress signaling.

**Design Philosophy:** Configuration lives in systemd unit drop-ins. Secrets use `systemd-creds`. The daemon reads configuration from environment variables set by systemd.

> **Implementation status.** This document describes the target design; the
> checkboxes below track what exists. Stage 1 is complete and Stage 2 is mostly
> complete. Two deliberate divergences remain: configuration and passwords live
> in the Secret Service rather than systemd credentials, and the daemon runs in
> the foreground. A third has been added since: everything now talks to a
> `RemoteStorage` trait rather than to WebDAV directly, so a second backend can
> be added without touching the layers above.
>
> See [../POC.md](../POC.md) for what actually works.

---

## Implementation Stages

### Stage 1: Core FUSE Mount (MVP)
- [x] Basic FUSE operations (getattr, readdir, readdirplus, lookup, open, statfs)
- [x] WebDAV client wrapper (reqwest + namespace-aware `PROPFIND`)
- [x] SQLite metadata store (id-keyed nodes, validators, TTL, stable inodes)
- [x] `read` — content is downloaded and served from a blob cache
- [x] File caching (download on open, keyed by identity + version)
- [ ] Environment-based configuration — uses Secret Service instead
- [ ] CLI: `davfs-sync daemon` (runs under systemd) — foreground `mount` only

### Stage 2: Sync State & Control
- [x] Sync state tracking (the `SyncState` column and vocabulary exist; only
      `Clean` is reachable until writes land)
- [x] Read-only xattrs (state, status, pinned, sizes, etag) — progress and
      directory aggregates still missing
- [x] Action xattrs (do_pin, do_unpin, do_download, do_free, do_refresh)
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
- [x] Nautilus extension (emblems from `user.davfs.state`; see [../nautilus-extension/](../nautilus-extension/))
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

The authoritative list is [`Cargo.toml`](../Cargo.toml). What the PoC currently
pulls in:

```toml
fuser = "0.14"                                              # FUSE
tokio = { version = "1", features = ["full"] }              # async runtime
reqwest = { version = "0.12", features = ["json"] }         # HTTP/WebDAV
http = "1.0"
url = "2.5"
urlencoding = "2.1"
sqlx = { version = "0.8", features = ["runtime-tokio", "sqlite"] }  # metadata cache
clap = { version = "4.5", features = ["derive"] }           # CLI
secret-service = { version = "4", features = ["rt-tokio-crypto-rust"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
toml = "0.8"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
anyhow = "1.0"
thiserror = "1.0"
chrono = { version = "0.4", features = ["serde"] }
libc = "0.2"
dirs = "5.0"
rpassword = "7.3"
```

Since added: `quick-xml` (the PROPFIND parser), `async-trait` (the backend
seam), `blake3` (content-cache keys), `bytes`/`futures`/`tokio-util` (streaming),
`percent-encoding` (replacing `urlencoding`), and `rand` (retry jitter).

Still to be added for the stages above:

```toml
sd-notify = "0.5"     # Stage 3: watchdog / readiness (pure Rust; no libsystemd)
globset = "0.4"       # Stage 4: glob rules
```

Note the design originally called for `rusqlite`; the implementation uses `sqlx`
with the async tokio driver so cache access can happen inside the async runtime.

---

## Next Steps

1. See [02-architecture.md](02-architecture.md) for system architecture and directory structure
2. See [03-configuration.md](03-configuration.md) for configuration details
3. See [04-cli.md](04-cli.md) for command-line interface specification
4. See [05-integration.md](05-integration.md) for file manager integration
