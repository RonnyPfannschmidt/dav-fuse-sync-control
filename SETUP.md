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

## Development Build

For faster compilation during development:
```bash
cargo build
# Binary at: target/debug/davfs-sync
```
