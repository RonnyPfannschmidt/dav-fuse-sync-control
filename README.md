# davfs-sync

WebDAV Virtual Filesystem with Offline Sync — a FUSE client for Linux that
presents a WebDAV share as a local directory, with metadata cached in SQLite so
directory navigation stays responsive.

**Status: read-only, and working.** Browsing and reading file content both work
over a real kernel mount, verified against Nextcloud and rclone. Writing does
not. The remaining design lives in [spec/](spec/).

## What works today

- WebDAV with HTTP Basic auth, namespace-aware `PROPFIND` parsing, and typed
  errors (a rate-limit no longer reaches applications as "file not found")
- **Reading file content**, cached on disk and keyed by (identity, version), so
  a changed file can never serve stale bytes
- Offline: anything already cached keeps working with the server unreachable;
  anything not cached fails `ENETUNREACH`, never `ENOENT`
- SQLite metadata cache with stable inodes across remounts, at
  `~/.local/share/davfs-sync/<name>/metadata.db`; blobs under
  `~/.cache/davfs-sync/<name>/`
- Stale-while-revalidate with conditional requests, plus a bounded breadth-first
  crawl at mount time
- Concurrent FUSE operations: handlers spawn rather than block, so one slow
  request no longer stalls the mount
- `readdirplus` and `statfs`
- Extended attributes: `state`, `status`, `pinned`, `remote_size`, `local_size`,
  `etag`, plus the `do_pin` / `do_unpin` / `do_download` / `do_free` /
  `do_refresh` actions
- A backend seam (`RemoteStorage`) shaped so a Google Drive client can drop in
- Credentials and mount config in the Secret Service (GNOME Keyring, KWallet)
- Import of an existing Nextcloud Desktop account (`setup-from-nextcloud`)
- Nautilus extension showing status emblems from the xattr

- systemd integration: `davfs-sync@.service` with `Type=notify` readiness, a
  watchdog, bounded restarts, and a drop-in generator
- A background worker: revalidation of the stalest directories, LRU cache
  eviction bounded by size and free disk, and online/offline tracking

## What does not work yet

- Read-only: write, create, delete and rename reply `EROFS`. The WebDAV verbs
  behind them are implemented and tested but deliberately not wired up.
- No glob rules or conflict handling
- Google Drive: the **client is implemented** and covered by mock tests, but it
  is not wired to a mount yet — the OAuth consent flow is still missing, and it
  has never run against real Drive. See
  [issue #1](https://github.com/RonnyPfannschmidt/dav-fuse-sync-control/issues/1).

## Quick start

```bash
cargo build --release

./target/release/davfs-sync setup mycloud \
  --url https://cloud.example.com/remote.php/dav/files/username/ \
  --username myuser \
  --mount-point ~/Cloud

./target/release/davfs-sync mount mycloud   # stays in foreground, Ctrl+C to unmount
```

Build prerequisites are in [SETUP.md](SETUP.md); a full walkthrough including the
Nextcloud import is in [POC.md](POC.md).

## Commands

| Command | Description |
|---------|-------------|
| `setup <name> --url --username --mount-point` | Configure a mount, prompting for the password |
| `setup-from-nextcloud <name> --mount-point [--remote-path]` | Configure from an installed Nextcloud Desktop account |
| `list` | List configured mounts |
| `mount <name> [--force]` | Mount in the foreground |
| `daemon` | Mount under systemd; configuration from `$DAVFS_*` |
| `start\|stop\|status <name>` | `systemctl --user` wrappers |

## Testing

```bash
cargo test --features testing        # 203 tests
```

Five tiers, each catching what the others cannot:

| Tier | Needs | Catches |
|---|---|---|
| unit + golden fixtures | nothing | parsing, naming, config, exit codes, unit files |
| `tests/vfs.rs` | nothing | caching, staleness, offline, single-flighting — no mount at all |
| `tests/webdav_live.rs`, `remote_seam.rs` | `rclone` | the client and the seam against a real server |
| `tests/e2e_fuse.rs` | `rclone`, `/dev/fuse` | the FUSE shims, through ordinary syscalls |
| `tests/gdrive_mock.rs` | nothing | the Drive client's HTTP and JSON mapping, OAuth refresh, 403-reason classification |
| `tests/nextcloud_live.rs` | a Nextcloud | the server this targets: `d:` prefixes, deep base paths, collection etags, real conditional requests |

Tiers that need a tool skip politely without it. `DAVFS_E2E_REQUIRE=1` and
`DAVFS_NEXTCLOUD_REQUIRE=1` turn a skip into a failure, which is what CI sets.

To run the Nextcloud tier locally:

```bash
podman run -d --name nc -p 18080:80 \
  -e SQLITE_DATABASE=nextcloud \
  -e NEXTCLOUD_ADMIN_USER=admin -e NEXTCLOUD_ADMIN_PASSWORD=adminpass123 \
  -e NEXTCLOUD_TRUSTED_DOMAINS='localhost 127.0.0.1' \
  docker.io/library/nextcloud:30-apache

DAVFS_NEXTCLOUD_URL=http://127.0.0.1:18080 \
  cargo test --features testing --test nextcloud_live -- --test-threads 1
```

## Running as a service

```bash
cp systemd/davfs-sync@.service systemd/davfs-sync.target ~/.config/systemd/user/
systemctl --user daemon-reload
davfs-sync setup mycloud --url ... --username ... --mount-point ~/Cloud
systemctl --user enable --now davfs-sync@mycloud.service
```

`setup` writes the drop-in. Before relying on it, run
[scripts/systemd-smoke.sh](scripts/systemd-smoke.sh) and the checklist in
[docs/systemd-checklist.md](docs/systemd-checklist.md) — in particular the check
that the mount is visible *outside* the service.

## Documentation

- **[POC.md](POC.md)** — current implementation: behaviour, walkthrough, limitations
- **[SETUP.md](SETUP.md)** — toolchain and system dependencies
- **[nautilus-extension/](nautilus-extension/)** — GNOME Files status emblems
- **[spec/](spec/)** — the target design (systemd units, pinning, sync worker,
  full xattr surface). Written before the PoC and mostly not implemented yet;
  read it as a plan, not a description of this code.
