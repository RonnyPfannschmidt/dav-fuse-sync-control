# davfs-sync: Specification

This directory contains the specification for **davfs-sync**, a FUSE-based WebDAV client for Linux with offline support and selective sync.

## Documents

1. **[01-overview.md](01-overview.md)** - Project overview, design philosophy, implementation stages, and dependencies
2. **[02-architecture.md](02-architecture.md)** - System architecture, project structure, systemd integration, and credential management
3. **[03-configuration.md](03-configuration.md)** - Environment variables, drop-in configuration, and config loader implementation
4. **[04-cli.md](04-cli.md)** - Command-line interface for `davfs-sync` and `davfs-ctl` tools
5. **[05-integration.md](05-integration.md)** - Extended attributes (xattrs) and file manager plugins

## Quick Start

### Reading Order

For new contributors or implementers, read in this order:
1. Start with [01-overview.md](01-overview.md) to understand the project goals and stages
2. Review [02-architecture.md](02-architecture.md) for the system design
3. Study [03-configuration.md](03-configuration.md) for configuration details
4. Check [04-cli.md](04-cli.md) for user-facing interfaces
5. Explore [05-integration.md](05-integration.md) for desktop integration

### Finding Information

- **Implementation stages**: [01-overview.md](01-overview.md)
- **Systemd setup**: [02-architecture.md](02-architecture.md)
- **Environment variables**: [03-configuration.md](03-configuration.md)
- **CLI commands**: [04-cli.md](04-cli.md)
- **Extended attributes**: [05-integration.md](05-integration.md)
- **File manager plugins**: [05-integration.md](05-integration.md)

## Design Philosophy

Configuration lives in systemd unit drop-ins. Secrets use `systemd-creds`. The daemon reads configuration from environment variables set by systemd.

## Project Status

This is a specification document for a project under development. See the implementation stages in [01-overview.md](01-overview.md) for current progress.
