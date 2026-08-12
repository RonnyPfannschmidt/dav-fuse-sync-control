# Setup Instructions

## Install Rust

```bash
# Install Rust using rustup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Reload your shell or run:
source "$HOME/.cargo/env"

# Verify installation
rustc --version
cargo --version
```

## Install System Dependencies

Both build headers and the runtime tools are needed: `davfs-sync mount` shells
out to `mountpoint` (util-linux) and `fusermount3` (fuse3).

### Fedora/RHEL
```bash
sudo dnf install fuse3-devel pkg-config openssl-devel dbus-devel
```

### Debian/Ubuntu
```bash
sudo apt install libfuse3-dev pkg-config libssl-dev libdbus-1-dev
```

### Arch Linux
```bash
sudo pacman -S fuse3 pkgconf openssl dbus
```

## Build the Project

```bash
cargo build --release
```

The binary will be at `target/release/davfs-sync`.

## Run Tests

```bash
cargo test
```

There is no test suite yet — this currently just compiles the crate.

## Runtime Requirements

- A Secret Service provider (GNOME Keyring or KWallet) reachable over D-Bus;
  every subcommand connects to it at startup
- Write access to `~/.local/share/davfs-sync/` for the cache database

## Development Build

For faster compilation during development:
```bash
cargo build
# Binary at: target/debug/davfs-sync
```
