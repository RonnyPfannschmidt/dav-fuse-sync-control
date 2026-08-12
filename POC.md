# davfs-sync: current implementation

A read-only WebDAV filesystem: it mirrors the remote directory tree into a FUSE
mount backed by a SQLite metadata cache, and downloads file content on demand
into an on-disk blob cache.

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

6. Read a file:
   ```bash
   cat ~/Cloud/somefile.txt
   ```
   The first read downloads and caches the content; later reads are local, and
   keep working with the server unreachable.

## How mounting works

1. Credentials and mount config are read from the Secret Service.
2. A `WebDavClient` is wrapped in a `WebDavBackend` and used behind the
   `RemoteStorage` trait; nothing above that layer knows it is WebDAV.
3. The connection is probed. A failure is reported but does not abort the
   mount -- cached data is still worth serving.
4. Two caches are opened: metadata at
   `~/.local/share/davfs-sync/<name>/metadata.db`, blobs under
   `~/.cache/davfs-sync/<name>/`. Migrations are applied and the root node is
   seeded. If the database belongs to a different account, it is wiped.
5. `initial_sync()` walks the tree breadth-first with `Depth: 1` listings, two
   levels deep, fetching each level concurrently.
6. The filesystem is mounted read-only with several event loop threads, each
   with its own fd.

## Cache behaviour

The metadata TTL is 30 seconds. Directory listings are always served from cache
when one exists, however old; if it is past its TTL a deduplicated background
revalidation is scheduled behind the answer. Only a directory that has never
been listed causes a blocking request. Revalidation is conditional, using the
directory's `getetag` from the previous response, so an unchanged directory
costs one 304.

File content is downloaded whole on first `open()` and stored as a blob keyed by
`blake3(node id + validator)`. Because the key includes the version, a changed
file cannot hit an old blob. Downloads are single-flighted: many concurrent
opens of one file produce one request. `open()` blocks until the content is
ready by default (`error` and `background` policies also exist), so failures
surface where applications expect them.

Content is written to `partial/`, fsync'd, and only then renamed into `blobs/`
and recorded in the database -- so the database can never point at a blob that
is not fully there. Blob handles are opened once, at `open()`, which is what
makes future eviction safe while readers are active.

## Extended attributes

Read-only:

| Attribute | Meaning |
|---|---|
| `user.davfs.state` | `cached` / `cloud` — for a file, whether content is local; for a directory, whether its listing is fresh |
| `user.davfs.status` | `remote` / `local` / `modified` / `syncing` / `conflict` / `error` |
| `user.davfs.pinned` | `1` or `0` |
| `user.davfs.remote_size` | server size, or `unknown` for a node with no size until exported |
| `user.davfs.local_size` | bytes cached locally |
| `user.davfs.etag` | the current validator |

Write to trigger (`setfattr`):

| Attribute | Action |
|---|---|
| `user.davfs.do_download` | fetch content now |
| `user.davfs.do_free` | drop cached content (refused if pinned) |
| `user.davfs.do_pin` / `do_unpin` | keep content locally |
| `user.davfs.do_refresh` | revalidate this directory now |

`user.davfs.state` keeps its exact vocabulary because the Nautilus extension
depends on it. The larger surface in
[spec/05-integration.md](spec/05-integration.md) (progress, directory
aggregates, recursive variants) is still unimplemented.

## Current limitations

- Read-only. `write`, `create`, `mkdir`, `unlink`, `rename` and `setattr` reply
  `EROFS`. The WebDAV verbs behind them (MKCOL, PUT, DELETE, MOVE) are
  implemented and tested, but nothing calls them.
- Conditional writes cannot be relied on universally. rclone's WebDAV server
  ignores `If-Match` entirely; Nextcloud honours it. Both branches are verified
  against real servers. See `Capabilities::enforces_preconditions`.
- No glob rules (pin/ignore/no-cache patterns) yet.
- The `user.davfs.state` xattr has no `progress` or directory-aggregate values.
- No Google Drive backend yet. The `RemoteStorage` seam is shaped for it --
  opaque node ids, duplicate names, unknown sizes, a cursor change feed -- but
  the client is not written, and it will need a per-user OAuth client ID.

## Running as a service

`davfs-sync daemon` is the systemd entry point; `davfs-sync mount <name>` is the
same code path in the foreground. The mount comes up *before* the initial crawl,
so `READY=1` fires as soon as the filesystem is usable and a large or
unreachable server cannot cause a start timeout.

The shipped unit deliberately omits three directives `spec/02` recommends,
because each is silently catastrophic — see
[docs/systemd-checklist.md](docs/systemd-checklist.md).

## Fixed since the proof of concept

Each of these was a real defect, and each now has a test:

| Was | Now |
|---|---|
| `read()` always returned `ENETUNREACH` | content is downloaded and cached |
| The parser returned **zero entries** against `rclone serve webdav` (it split on the literal `<d:response>`; rclone emits `<D:response>`, on one line) | namespace-aware parsing; prefix-independent |
| Every directory listed itself as its own child | the `Depth: 1` self-entry is separated out |
| Conditional requests never fired (a 207 carries no `ETag` header, so the validator was always `None`) | the validator comes from the self-entry's `<getetag>` in the body |
| 304 was signalled by an empty `Vec`, so deleting the last file in a directory never propagated | an explicit `ListResult`/`Listing` variant |
| `Depth: infinity`, which Nextcloud and SabreDAV reject with 403 | a bounded breadth-first crawl of `Depth: 1` listings |
| mtimes were parsed, stored, then reported as the Unix epoch | real mtimes reach the kernel |
| Directories could never report `cached` (the check was `path.ends_with('/')`, never true) | state is derived from the node's kind |
| Every failure became `ENOENT`, so a rate-limit told applications the file was gone | typed errors with a tested errno mapping |
| Inode numbers changed on every mount | persisted, stable across remounts |
| Every FUSE operation was serialized on one thread | handlers spawn; several event loops |
| `chrono::DateTime::from_timestamp(..).unwrap()` panicked the filesystem on a corrupt timestamp | degrades to `None` |
| `INSERT OR REPLACE` cascaded a directory's children away | `ON CONFLICT DO UPDATE` |
| XML entities passed through raw (`apostrophe&#39;s.txt`) | decoded |
| `Url::join` dropped a base segment and mangled `#`/`?`/`%` in names | URLs built from path segments |
| `statfs` was `ENOSYS`, breaking `df` and some file choosers | implemented |
| `process::exit(0)` from a signal handler skipped every `Drop`, leaving SQLite mid-WAL | ordered shutdown: unmount, join, checkpoint |
| The mount point was force-unmounted on every start, which under `Restart=` could tear down a *different* live mount | only with an explicit `mount --force` |

## Next steps

See [spec/](spec/) for the full specification of the intended implementation.
