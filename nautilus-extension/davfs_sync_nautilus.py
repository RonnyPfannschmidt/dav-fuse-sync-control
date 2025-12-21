#!/usr/bin/env python3
"""
Nautilus extension for davfs-sync
Shows sync status emblems on files in mounted WebDAV directories
"""

import os
import time
import gi
gi.require_version('Nautilus', '4.1')
from gi.repository import Nautilus, GObject

class DavfsSyncExtension(GObject.GObject, Nautilus.InfoProvider, Nautilus.MenuProvider):
    """
    Nautilus extension that shows sync status for davfs-sync mounted directories
    """
    
    def __init__(self):
        super().__init__()
        self.mount_points = []
        self.mount_points_cache_time = 0
        self.mount_points_cache_ttl = 5.0  # Refresh every 5 seconds
        self._refresh_mount_points()
    
    def _refresh_mount_points(self):
        """Refresh mount points cache if stale"""
        now = time.time()
        if now - self.mount_points_cache_time > self.mount_points_cache_ttl:
            self.mount_points = self._get_mount_points()
            self.mount_points_cache_time = now
    
    def _get_mount_points(self):
        """Get list of davfs-sync mount points from /proc/mounts"""
        mount_points = []
        try:
            with open('/proc/mounts', 'r') as f:
                for line in f:
                    if 'davfs-sync' in line or 'fuse' in line:
                        parts = line.split()
                        if len(parts) >= 2:
                            mount_point = parts[1]
                            # Decode escaped characters in mount path
                            mount_point = mount_point.replace('\\040', ' ')
                            mount_points.append(mount_point)
        except Exception as e:
            print(f"Error reading mount points: {e}")
        
        return mount_points
    
    def _is_in_davfs_mount(self, file_path):
        """Check if file is in a davfs-sync mount point"""
        # Quick check: if no mount points, bail early
        if not self.mount_points:
            return False
        
        for mount_point in self.mount_points:
            if file_path.startswith(mount_point):
                return True
        return False
    
    def _get_sync_state(self, file_path):
        """
        Determine sync state of a file by reading xattr from the filesystem
        Fast path: use heuristics first, only read xattr if needed
        """
        if not self._is_in_davfs_mount(file_path):
            return None
        
        # Use heuristics first for speed (most common cases)
        is_dir = os.path.isdir(file_path)
        
        # Try to read state from xattr using built-in os module
        # Use non-blocking approach with timeout
        try:
            state = os.getxattr(file_path, 'user.davfs.state', follow_symlinks=False).decode('utf-8')
            return state  # 'cached', 'cloud', or 'unknown'
        except (OSError, IOError, AttributeError):
            # Xattr not available or error reading - use fast fallback
            pass
        
        # Fast fallback: assume files are cloud, directories might be cached
        return 'cached' if is_dir else 'cloud'
    
    def update_file_info(self, file):
        """Called by Nautilus to update file information"""
        # CRITICAL: Return as fast as possible to not block Nautilus UI
        
        # Quick bailout checks first (no I/O)
        if file.get_uri_scheme() != 'file':
            return Nautilus.OperationResult.COMPLETE
        
        file_path = file.get_location().get_path()
        if not file_path:
            return Nautilus.OperationResult.COMPLETE
        
        # Refresh mount points cache if needed (only every 5 seconds)
        self._refresh_mount_points()
        
        # Quick check: bail early if not in any mount (no I/O, just string comparison)
        if not self._is_in_davfs_mount(file_path):
            return Nautilus.OperationResult.COMPLETE
        
        # For davfs mounts: read sync state from xattr
        # FUSE serves stale cache so this should be fast now
        try:
            state = self._get_sync_state(file_path)
            
            if state == 'cached':
                # Directory listing is cached or this is root
                file.add_emblem('emblem-default')  # Checkmark
                file.add_string_attribute('davfs_sync_status', 'WebDAV Directory (cached)')
            elif state == 'cloud':
                # File exists in cloud (metadata cached, content not)
                file.add_emblem('emblem-web')  # Cloud icon
                file.add_string_attribute('davfs_sync_status', 'Available Online')
            # For 'unknown' state, don't add any emblem
        except Exception as e:
            # Fallback: add generic web emblem
            print(f"DavFS emblem error: {e}")
            file.add_emblem('emblem-web')
        
        return Nautilus.OperationResult.COMPLETE
    
    def get_file_items(self, files):
        """Add context menu items for files"""
        if len(files) != 1:
            return []
        
        file = files[0]
        if file.get_uri_scheme() != 'file':
            return []
        
        file_path = file.get_location().get_path()
        if not file_path or not self._is_in_davfs_mount(file_path):
            return []
        
        # Add menu item for sync actions
        menu_item = Nautilus.MenuItem(
            name='DavfsSyncExtension::Info',
            label='WebDAV Info',
            tip='Show WebDAV mount information'
        )
        menu_item.connect('activate', self._show_sync_info, file_path)
        
        return [menu_item]
    
    def _show_sync_info(self, menu, file_path):
        """Show sync information dialog"""
        try:
            # Find which mount point this file belongs to
            mount_point = None
            for mp in self.mount_points:
                if file_path.startswith(mp):
                    mount_point = mp
                    break
            
            if mount_point:
                os.system(f'notify-send "WebDAV Mount" "File is in WebDAV mount at: {mount_point}\n\nNote: Files are NOT downloaded locally.\nThis is a read-only online view." -i folder-remote')
            else:
                os.system('notify-send "WebDAV Mount" "File is not in a WebDAV mount" -i dialog-information')
        except Exception as e:
            print(f"Error showing sync info: {e}")
