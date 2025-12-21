# davfs-sync: Integration

## Extended Attributes (xattrs)

Extended attributes provide a programmatic interface to query file status and trigger actions.

### Read-Only Status Attributes

| Attribute | Description |
|-----------|-------------|
| `user.davfs.status` | `remote`/`syncing`/`local`/`modified`/`conflict`/`error` |
| `user.davfs.pinned` | `1` or `0` |
| `user.davfs.progress` | `0.00` to `1.00` (when syncing) |
| `user.davfs.progress_bytes` | `downloaded/total` |
| `user.davfs.remote_size` | Server size in bytes |
| `user.davfs.local_size` | Cached size in bytes |
| `user.davfs.etag` | Server ETag |
| `user.davfs.error` | Error message (when status=error) |
| `user.davfs.rule_match` | Matched glob pattern |

### Directory Aggregates (Read-Only)

| Attribute | Description |
|-----------|-------------|
| `user.davfs.sync_active` | Count of active syncs under dir |
| `user.davfs.sync_pending` | Count of pending syncs |
| `user.davfs.sync_files` | JSON array of syncing paths |
| `user.davfs.sync_bytes_total` | Total bytes being synced |
| `user.davfs.sync_bytes_done` | Completed bytes |
| `user.davfs.children_local` | Local children count |
| `user.davfs.children_remote` | Remote children count |

### Action Triggers (Write-Only)

Write any value to trigger. Reading returns `ENODATA`.

| Attribute | Description |
|-----------|-------------|
| `user.davfs.do_pin` | Pin file/dir |
| `user.davfs.do_pin_r` | Pin recursively |
| `user.davfs.do_unpin` | Unpin |
| `user.davfs.do_unpin_r` | Unpin recursively |
| `user.davfs.do_download` | Download now |
| `user.davfs.do_download_r` | Download recursively |
| `user.davfs.do_free` | Free cache |
| `user.davfs.do_free_r` | Free recursively |
| `user.davfs.do_refresh` | Refresh metadata |
| `user.davfs.do_refresh_r` | Refresh recursively |
| `user.davfs.do_upload` | Force upload |
| `user.davfs.do_cancel` | Cancel transfer |

### Usage Examples

```bash
# Check status
$ getfattr -n user.davfs.status ~/Cloud/file.pdf
user.davfs.status="remote"

# Trigger download
$ setfattr -n user.davfs.do_download -v 1 ~/Cloud/file.pdf

# Check progress
$ getfattr -n user.davfs.progress ~/Cloud/file.pdf
user.davfs.progress="0.45"

# See what's syncing in a folder
$ getfattr -n user.davfs.sync_files ~/Cloud/Projects/
user.davfs.sync_files="[\"Projects/a.zip\",\"Projects/b.tar\"]"

# List all davfs attrs
$ getfattr -d -m "user.davfs.*" ~/Cloud/file.pdf
```

---

## File Manager Plugins

### Nautilus Extension (Python)

```python
# ~/.local/share/nautilus-python/extensions/davfs-sync.py

from gi.repository import Nautilus, GObject
import os

class DavFSSyncExtension(GObject.GObject, Nautilus.MenuProvider):
    def _is_davfs(self, path):
        try:
            os.getxattr(path, "user.davfs.status")
            return True
        except:
            return False
    
    def get_file_items(self, files):
        if len(files) != 1:
            return []
        
        path = files[0].get_location().get_path()
        if not self._is_davfs(path):
            return []
        
        status = os.getxattr(path, "user.davfs.status").decode()
        is_dir = files[0].is_directory()
        
        items = []
        
        if status == "remote":
            item = Nautilus.MenuItem(
                name="DavFS::Download",
                label="Download from Cloud",
                icon="cloud-download"
            )
            attr = "user.davfs.do_download_r" if is_dir else "user.davfs.do_download"
            item.connect("activate", lambda *_: os.setxattr(path, attr, b"1"))
            items.append(item)
        
        # Add more menu items based on status
        
        return items
```

### Dolphin Service Menu (KDE)

```ini
# ~/.local/share/kio/servicemenus/davfs-sync.desktop

[Desktop Entry]
Type=Service
MimeType=all/allfiles;inode/directory;
Actions=Download;Pin;Free;

[Desktop Action Download]
Name=Download from Cloud
Icon=cloud-download
Exec=setfattr -n user.davfs.do_download -v 1 %f

[Desktop Action Pin]
Name=Keep Offline
Icon=folder-favorites
Exec=setfattr -n user.davfs.do_pin -v 1 %f

[Desktop Action Free]
Name=Free Space
Icon=edit-clear
Exec=setfattr -n user.davfs.do_free -v 1 %f
```

### Nemo Actions

```ini
# ~/.local/share/nemo/actions/davfs-sync-download.nemo_action

[Nemo Action]
Name=Download from Cloud
Comment=Download this file/folder from WebDAV
Exec=setfattr -n user.davfs.do_download -v 1 %F
Icon-Name=cloud-download
Selection=Any
Extensions=any;
Conditions=exec getfattr -n user.davfs.status %F 2>/dev/null;
```

### Status Emblems

File managers can display sync status using emblems/badges by monitoring xattrs:

- **Remote**: Cloud icon
- **Syncing**: Progress indicator
- **Local**: Checkmark or offline icon
- **Pinned**: Pin/star icon
- **Conflict**: Warning icon
- **Error**: Error icon

Implementation varies by file manager and requires platform-specific APIs.
