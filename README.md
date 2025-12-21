# davfs-sync

WebDAV Virtual Filesystem with Offline Sync

## Proof of Concept

A working PoC is available! See [POC.md](POC.md) for instructions.

The PoC demonstrates:
- WebDAV server connection and directory listing
- Configuration storage in Secret Service (GNOME Keyring/KWallet)
- FUSE filesystem mounting (read-only)
- Foreground CLI operation
- Network error simulation for file reads

## Documentation

The project specification has been organized into the [spec/](spec/) directory:

- **[spec/README.md](spec/README.md)** - Specification index and reading guide
- **[spec/01-overview.md](spec/01-overview.md)** - Project overview and implementation stages
- **[spec/02-architecture.md](spec/02-architecture.md)** - System architecture and directory layout
- **[spec/03-configuration.md](spec/03-configuration.md)** - Configuration and environment variables
- **[spec/04-cli.md](spec/04-cli.md)** - Command-line interface reference
- **[spec/05-integration.md](spec/05-integration.md)** - Extended attributes and file manager plugins

## Quick Start

See the [spec/](spec/) directory for complete documentation.
