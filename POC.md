# davfs-sync Proof of Concept

The current implementation is a read-only WebDAV browser: it mirrors the remote
directory tree into a FUSE mount, backed by a SQLite metadata cache. File
*content* is never fetched.

## Features

- Browses the full remote directory tree, not just the root
- Real file sizes and directory/file types from `PROPFIND` properties
- SQLite metadata cache with ETag-based conditional revalidation
- Recursive prefetch at mount time, so the first `ls` is served from cache
- Stale-while-revalidate on every FUSE call: cached data is returned immediately
  and a background refresh is scheduled if the entry is past its TTL
- Configuration and password stored in the Secret Service (GNOME Keyring, KWallet)
- Can adopt an existing Nextcloud Desktop account's URL, username, and password
- Exposes `user.davfs.state` so file managers can show sync emblems
- CLI stays in the foreground while mounted; Ctrl+C unmounts

## Prerequisites

- Rust 1.70 or later
- FUSE 3 development files (`libfuse3-dev` on Debian/Ubuntu) and the
  `fusermount3` binary at runtime
- Secret Service provider (GNOME Keyring or KWallet)
- D-Bus running

See [SETUP.md](SETUP.md) for per-distribution package names.

## Setup

1. Build the project:
   ```bash
   cargo build --release
   ```

2. Configure a mount:
   ```bash
   ./target/release/davfs-sync setup mycloud \
     --url https://cloud.example.com/remote.php/dav/files/username/ \
     --username myuser \
     --mount-point ~/Cloud
   ```
   You'll be prompted for the password.

   Or, if Nextcloud Desktop is already set up on this machine:
   ```bash
   ./target/release/davfs-sync setup-from-nextcloud mycloud \
     --mount-point ~/Cloud \
     --remote-path /
   ```
   This reads `nextcloud.cfg` (native or Flatpak location) for the server URL and
   username, and tries the keyring for the password, falling back to a prompt.

3. List configured mounts:
   ```bash
   ./target/release/davfs-sync list
   ```

4. Mount the filesystem:
   ```bash
   ./target/release/davfs-sync mount mycloud
   ```

   The CLI will stay in the foreground. Press Ctrl+C to unmount.

5. In another terminal, browse:
   ```bash
   ls ~/Cloud
   ls ~/Cloud/Documents
   getfattr -n user.davfs.state ~/Cloud/Documents
   ```

6. Try to read a file (this fails by design):
   ```bash
   cat ~/Cloud/somefile.txt
   # cat: /home/user/Cloud/somefile.txt: Network is unreachable
   ```

## How mounting works

1. If the mount point is already mounted, `fusermount3 -u` (then `-uz`) is run to
   clear it.
2. The WebDAV connection is tested with a `Depth: 0` `PROPFIND`. A failure is
   reported but does not abort the mount.
3. The cache database is opened at
   `~/.local/share/davfs-sync/<name>.db` and migrations are applied.
4. `initial_sync()` issues one `Depth: infinity` `PROPFIND`, keeps entries up to
   3 path levels deep, and writes every directory listing to the cache. A failure
   here is reported and the mount continues with whatever is already cached.
5. The FUSE filesystem is mounted read-only with `fuser::mount2`.

## Cache behaviour

The cache TTL is 5 seconds (hardcoded in `mount_filesystem`).

`lookup`, `getattr`, and `readdir` all follow the same path: read the cached
listing regardless of age, then call `schedule_refresh()`. That helper skips the
work if a refresh for the same path is already in flight or if the entry is still
within its TTL; otherwise it spawns a task that revalidates with `If-None-Match`
and rewrites the cache on a 200. Only a completely uncached directory causes a
blocking WebDAV request.

## Extended attributes

`user.davfs.state` is the only attribute implemented:

| Value | Meaning |
|-------|---------|
| `cached` | Directory with a listing inside its TTL (the root is always `cached`) |
| `cloud` | Metadata known, content not local — every file, and directories with a stale listing |
| `unknown` | Nothing cached for this path or its parent |

The spec's much larger xattr surface (`user.davfs.status`, `do_pin`, progress,
directory aggregates) is not implemented — see [spec/05-integration.md](spec/05-integration.md).

## Current limitations

- `read()` unconditionally returns `ENETUNREACH`; no file content is downloaded
  or cached
- Read-only mount: no write, create, delete, rename, or `setxattr`
- `mtime`/`atime`/`ctime` are reported as the Unix epoch even though `PROPFIND`
  timestamps are parsed and stored
- WebDAV XML is parsed by line-oriented string matching, and only handles the
  `d:` namespace prefix
- Initial sync depth is capped at 3 levels; deeper directories are fetched
  on demand
- Inode numbers are assigned in memory and never reused or reclaimed, so they
  change across mounts
- The cache database is never pruned or size-bounded
- No systemd unit; the process must stay running

## Next steps

See [spec/](spec/) for the full specification of the intended implementation.
