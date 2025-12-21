# Nautilus Extension for davfs-sync

This directory contains a Nautilus (GNOME Files) extension that shows status emblems on files in davfs-sync mounted directories.

## Features

- Shows web/cloud emblem on files and folders (indicating they're available online but not downloaded)
- Adds context menu item "WebDAV Info" to show mount information
- Automatically detects davfs-sync mount points
- Displays accurate status: files are NOT downloaded locally (PoC read-only mount)

## Installation

### For Current User Only

1. Install required dependencies:
   ```bash
   # On Fedora/RHEL
   sudo dnf install python3-nautilus
   
   # On Ubuntu/Debian
   sudo apt install python3-nautilus
   ```

2. Create the Nautilus extensions directory if it doesn't exist:
   ```bash
   mkdir -p ~/.local/share/nautilus-python/extensions
   ```

3. Copy the extension:
   ```bash
   cp nautilus-extension/davfs_sync_nautilus.py ~/.local/share/nautilus-python/extensions/
   ```

4. Restart Nautilus:
   ```bash
   nautilus -q
   ```

### For All Users (System-wide)

1. Install as root:
   ```bash
   sudo cp nautilus-extension/davfs_sync_nautilus.py /usr/share/nautilus-python/extensions/
   ```

2. Restart Nautilus for each user:
   ```bash
   nautilus -q
   ```

## Usage

Once installed:

1. Mount a WebDAV filesystem using davfs-sync:
   ```bash
   davfs-sync mount next-dav
   ```

2. Open the mount point in Nautilus (GNOME Files)

3. Files and folders will show web/cloud emblems indicating they are:
   - **Available online only** (not downloaded locally)
   - **Read-only** (this is a PoC mount)

4. Right-click on any file in the mount to see "WebDAV Info" in the context menu

## Troubleshooting

### Extension not loading

1. Check if nautilus-python is installed:
   ```bash
   python3 -c "import gi; gi.require_version('Nautilus', '4.1'); from gi.repository import Nautilus"
   ```

2. Check Nautilus version (extension requires Nautilus 4.1+):
   ```bash
   nautilus --version
   ```

3. Enable debug output:
   ```bash
   NAUTILUS_PYTHON_DEBUG=misc nautilus
   ```

4. Check extension is in the right location:
   ```bash
   ls -la ~/.local/share/nautilus-python/extensions/
   ```

### Emblems not showing

1. Make sure the filesystem is actually mounted:
   ```bash
   mount | grep davfs-sync
   ```

2. Check the extension is loaded:
   ```bash
   # Look for davfs_sync_nautilus in the output
   NAUTILUS_PYTHON_DEBUG=misc nautilus 2>&1 | grep davfs
   ```

3. Try clearing Nautilus cache:
   ```bash
   rm -rf ~/.cache/nautilus
   nautilus -q
   ```

## Future Enhancements

- [ ] Add D-Bus interface to query real sync state from davfs-sync daemon
- [ ] Show different emblems for different states (syncing, conflict, error)
- [ ] Add progress indicator for syncing files
- [ ] Add menu actions to force sync or resolve conflicts
- [ ] Show additional file properties (last sync time, remote URL, etc.)
- [ ] Support for Nemo (Cinnamon) and other file managers

## Development

To modify the extension:

1. Edit the Python file
2. Restart Nautilus: `nautilus -q`
3. Check for errors: `journalctl --user -f | grep nautilus`

The extension uses:
- `Nautilus.InfoProvider`: To add emblems and attributes
- `Nautilus.MenuProvider`: To add context menu items

For more information on Nautilus extensions, see:
- https://wiki.gnome.org/Projects/NautilusPython
- https://github.com/GNOME/nautilus-python
