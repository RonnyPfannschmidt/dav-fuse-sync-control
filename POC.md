# davfs-sync Proof of Concept

This is a proof-of-concept implementation of a WebDAV FUSE filesystem.

## Features

- Lists files from a WebDAV server
- Stores configuration in Secret Service (GNOME Keyring, KWallet)
- Returns "no network" error when trying to read file content (PoC limitation)
- CLI stays in foreground while mounted

## Prerequisites

- Rust 1.70 or later
- FUSE library (`libfuse3-dev` on Debian/Ubuntu)
- Secret Service provider (GNOME Keyring or KWallet)
- D-Bus running

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

3. List configured mounts:
   ```bash
   ./target/release/davfs-sync list
   ```

4. Mount the filesystem:
   ```bash
   ./target/release/davfs-sync mount mycloud
   ```
   
   The CLI will stay in the foreground. Press Ctrl+C to unmount.

5. In another terminal, list files:
   ```bash
   ls ~/Cloud
   ```

6. Try to read a file (will fail with network error):
   ```bash
   cat ~/Cloud/somefile.txt
   # Returns: cat: /home/user/Cloud/somefile.txt: Network is unreachable
   ```

## Current Limitations (PoC)

- Read operations always return "Network unreachable" error
- No caching
- No write support
- Simplified WebDAV XML parsing
- Only lists root directory
- No subdirectory support
- No proper file size/timestamp handling

## Next Steps

See [spec/](spec/) directory for the full specification of the intended implementation.
