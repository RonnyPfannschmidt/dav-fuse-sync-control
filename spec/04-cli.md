# davfs-sync: Command-Line Interface

## Command Structure

All operations use the `davfs-sync` command with subcommands:

```
davfs-sync <subcommand> [options] [arguments]
```

### Quick Reference

**Setup & Management:**
- `setup` - Configure new mount
- `list` - List configured mounts
- `show` - Show mount configuration
- `edit` - Edit mount configuration
- `remove` - Remove mount configuration
- `set-password` - Update mount password

**Service Control:**
- `start/stop/restart` - Control mount service
- `enable/disable` - Configure auto-start
- `status` - Show service status
- `logs` - View service logs

**File Operations:**
- `status` - Check file/directory sync status
- `pin/unpin` - Keep files offline
- `download` - Download without pinning
- `free` - Free cache space
- `cancel` - Cancel active transfer
- `find` - Find files by status

**Monitoring:**
- `watch` - Monitor active transfers
- `queue` - Show sync queue
- `cache` - Show cache usage

**Rules & Conflicts:**
- `rule` - Manage glob rules
- `check-rules` - Test rule matching
- `conflicts` - List conflicts
- `resolve` - Resolve conflicts

---

## Setup Commands

```bash
# Interactive setup - creates drop-in + credential
$ davfs-sync setup personal
WebDAV URL: https://cloud.example.com/remote.php/dav/files/user/
Username: myuser
Password: ********
Mount point [~/Cloud/Personal]: 

Creating: ~/.config/systemd/user/davfs-sync@personal.service.d/mount.conf
Creating: ~/.config/davfs-sync/credentials/personal.cred
Creating: ~/.cache/davfs-sync/personal/
Creating: ~/.local/share/davfs-sync/personal/
Creating: ~/Cloud/Personal/

Done! Commands:
  systemctl --user daemon-reload
  systemctl --user start davfs-sync@personal
  systemctl --user enable davfs-sync@personal   # start at login

# Non-interactive
$ davfs-sync setup work \
    --url "https://work.com/dav/" \
    --username "me@work.com" \
    --mount "~/Cloud/Work" \
    --password-stdin < /path/to/pw

# With options
$ davfs-sync setup media \
    --url "https://nas/dav/" \
    --username "user" \
    --mount "~/Media" \
    --cache-max-gb 50 \
    --on-open-uncached block \
    --rule-pin "Favorites/**" \
    --rule-no-cache "**"
```

## Management Commands

```bash
# List configured mounts
$ davfs-sync list
NAME      URL                              MOUNT              STATUS   ENABLED
personal  https://cloud.example.com/...    ~/Cloud/Personal   active   yes
work      https://work.com/dav/            ~/Cloud/Work       active   yes  
media     https://nas/dav/                 ~/Media            inactive no

# Show configuration
$ davfs-sync show personal
Name: personal
URL: https://cloud.example.com/remote.php/dav/files/user/
Username: myuser
Mount: /home/user/Cloud/Personal
Cache: 20 GB max, evict after 30 days
Rules:
  pin: Documents/Important/**
  ignore: *.tmp; Trash/**

# Edit drop-in (opens $EDITOR)
$ davfs-sync edit personal

# Update password
$ davfs-sync set-password personal

# Remove mount
$ davfs-sync remove personal
This will:
  - Stop the service
  - Remove drop-in configuration
  - Remove credential
  - Keep cache and metadata (use --purge to delete)
Proceed? [y/N]: y
```

## Service Control

Wraps systemctl for convenience:

```bash
$ davfs-sync start personal
$ davfs-sync stop personal
$ davfs-sync restart personal
$ davfs-sync enable personal      # start at login
$ davfs-sync disable personal
$ davfs-sync status               # all mounts
$ davfs-sync status personal      # specific mount
$ davfs-sync logs personal        # journalctl wrapper
$ davfs-sync logs personal -f     # follow
```

## Rule Management

```bash
# Add rules
$ davfs-sync rule personal --add-pin "Projects/**"
$ davfs-sync rule personal --add-ignore "*.log"
$ davfs-sync rule personal --add-no-cache "Videos/**"

# Remove rules  
$ davfs-sync rule personal --remove-pin "Projects/**"

# List rules
$ davfs-sync rule personal --list
pin:
  - Documents/Important/**
  - Projects/**
ignore:
  - *.tmp
  - *.log
no_cache:
  - Videos/**
max_size_mb: 100
```

---

## File Operations

### Status Commands

```bash
# Status (auto-detects mount from path)
$ davfs-sync status ~/Cloud/Personal/Documents/file.pdf
$ davfs-sync status -r ~/Cloud/Personal/         # recursive

# Find by status
$ davfs-sync find --local ~/Cloud/
$ davfs-sync find --remote ~/Cloud/
$ davfs-sync find --pinned ~/Cloud/
```

### Sync Commands

```bash
# Pin/unpin (keep offline)
$ davfs-sync pin ~/Cloud/file.pdf
$ davfs-sync pin -r ~/Cloud/Projects/
$ davfs-sync unpin ~/Cloud/old/

# Download (without pinning)
$ davfs-sync download ~/Cloud/file.pdf
$ davfs-sync download -r ~/Cloud/Archive/

# Free cache
$ davfs-sync free ~/Cloud/big.iso
$ davfs-sync free -r ~/Cloud/old-backups/

# Cancel transfer
$ davfs-sync cancel ~/Cloud/file.pdf
```

### Monitoring Commands

```bash
# Watch active transfers
$ davfs-sync watch
$ davfs-sync watch --mount personal

# Queue info
$ davfs-sync queue
$ davfs-sync queue --mount personal

# Cache usage
$ davfs-sync cache
$ davfs-sync cache --mount personal
```

### Rule Testing

```bash
# Test glob rules
$ davfs-sync check-rules ~/Cloud/some/file.pdf
Path: some/file.pdf
Matched: pin
Pattern: **/*.pdf
Action: Will be auto-pinned
```

### Conflict Resolution

```bash
# List conflicts
$ davfs-sync conflicts

# Resolve conflicts
$ davfs-sync resolve --keep-local ~/Cloud/file.pdf
$ davfs-sync resolve --keep-remote ~/Cloud/file.pdf
```
