# davfs-sync

WebDAV Virtual Filesystem with Offline Sync — a FUSE client for Linux that
presents a WebDAV share as a local directory, with metadata cached in SQLite so
directory navigation stays responsive.

**Status: read-only, and working.** Browsing and reading file content both
work, over a real kernel mount. Writing does not. The remaining design lives in
[spec/](spec/).

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

## What does not work yet

- Read-only: write, create, delete and rename reply `EROFS`. The WebDAV verbs
  behind them are implemented and tested but deliberately not wired up.
- No systemd integration; `mount` runs in the foreground only
- No background sync worker, cache eviction, glob rules or conflict handling
- No Google Drive backend (the seam is ready; the client is not written)

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
| `mount <name>` | Mount in the foreground |

## Testing

```bash
cargo test --features testing        # 154 tests
```

Four tiers, each catching what the others cannot: unit tests, golden fixtures of
real server responses, the core exercised against an in-memory backend with no
mount at all, and a real FUSE mount over a real `rclone serve webdav`. The last
two need `rclone`; the FUSE tier also needs `/dev/fuse`, and both skip politely
without them.

## Documentation

- **[POC.md](POC.md)** — current implementation: behaviour, walkthrough, limitations
- **[SETUP.md](SETUP.md)** — toolchain and system dependencies
- **[nautilus-extension/](nautilus-extension/)** — GNOME Files status emblems
- **[spec/](spec/)** — the target design (systemd units, pinning, sync worker,
  full xattr surface). Written before the PoC and mostly not implemented yet;
  read it as a plan, not a description of this code.
