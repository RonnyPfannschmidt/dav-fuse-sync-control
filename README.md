# davfs-sync

WebDAV Virtual Filesystem with Offline Sync — a FUSE client for Linux that
presents a WebDAV share as a local directory, with metadata cached in SQLite so
directory navigation stays responsive.

**Status: proof of concept.** Directory browsing works; reading file *content*
does not (see [POC.md](POC.md)). The full intended design lives in [spec/](spec/)
and is largely unimplemented.

## What works today

- WebDAV connection with HTTP Basic auth and `PROPFIND` directory listings
- Recursive `PROPFIND` (`Depth: infinity`, capped at 3 path levels) on mount to
  populate the cache before the filesystem is exposed
- SQLite metadata cache (`directories` + `entries` tables, 5 s TTL) at
  `~/.local/share/davfs-sync/<name>.db`
- Stale-while-revalidate: FUSE calls answer from cache immediately and schedule a
  deduplicated background refresh using conditional `If-None-Match` requests
- Nested directory navigation (`lookup`, `getattr`, `readdir`), read-only mount
- `user.davfs.state` extended attribute (`cached` / `cloud` / `unknown`)
- Credentials and mount config in the Secret Service (GNOME Keyring, KWallet)
- Import of an existing Nextcloud Desktop account (`setup-from-nextcloud`)
- Nautilus extension showing status emblems from the xattr

## What does not work yet

- `read()` always fails with `ENETUNREACH` — no file content is ever downloaded
- Read-only: no write, create, delete, or rename
- No pinning, selective sync, glob rules, or conflict handling
- No systemd integration; `mount` runs in the foreground only
- WebDAV XML is parsed with string matching, not an XML parser

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

## Documentation

- **[POC.md](POC.md)** — current implementation: behaviour, walkthrough, limitations
- **[SETUP.md](SETUP.md)** — toolchain and system dependencies
- **[nautilus-extension/](nautilus-extension/)** — GNOME Files status emblems
- **[spec/](spec/)** — the target design (systemd units, pinning, sync worker,
  full xattr surface). Written before the PoC and mostly not implemented yet;
  read it as a plan, not a description of this code.
