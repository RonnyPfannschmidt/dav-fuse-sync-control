use fuser::{
    Errno, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner, OpenFlags,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, ReplyXattr, Request,
};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::fuse::attr::{to_file_attr, NodeMeta, OwnerIds};
use crate::store::MetaStore;
use crate::webdav::{DavEntry, ListResult, WebDavClient};

const TTL: Duration = Duration::from_secs(1);

const ROOT_INO: u64 = 1;

pub struct DavFS {
    webdav: WebDavClient,
    /// Captured explicitly rather than relying on `Handle::current()`.
    ///
    /// The PoC's FUSE callbacks called `Handle::current()`, which only worked
    /// because `mount2` happened to be invoked inside `spawn_blocking`. Mounting
    /// from a plain thread would panic, and a `current_thread` runtime would
    /// deadlock -- which made the filesystem untestable.
    rt: tokio::runtime::Handle,
    /// Ownership reported for every node.
    owner: OwnerIds,
    // Map inode to path
    inode_to_path: Arc<Mutex<HashMap<u64, String>>>,
    // Map path to inode
    path_to_inode: Arc<Mutex<HashMap<String, u64>>>,
    next_inode: Arc<Mutex<u64>>,
    // Directory listing cache
    dir_cache: MetaStore,
    // Track paths currently being refreshed to avoid duplicates
    refresh_in_progress: Arc<Mutex<HashSet<String>>>,
}

impl DavFS {
    pub fn new(
        rt: tokio::runtime::Handle,
        webdav: WebDavClient,
        dir_cache: MetaStore,
        owner: OwnerIds,
    ) -> Self {
        let mut inode_to_path = HashMap::new();
        let path_to_inode = HashMap::new();

        // Root directory is at /
        inode_to_path.insert(ROOT_INO, String::from("/"));

        Self {
            webdav,
            rt,
            owner,
            inode_to_path: Arc::new(Mutex::new(inode_to_path)),
            path_to_inode: Arc::new(Mutex::new(path_to_inode)),
            next_inode: Arc::new(Mutex::new(2)),
            dir_cache,
            refresh_in_progress: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Schedule a background refresh for a path if not already in progress
    /// Returns true if refresh was scheduled, false if already in progress or not needed
    fn schedule_refresh(&self, path: &str) -> bool {
        // Check if already refreshing this path
        {
            let in_progress = self.refresh_in_progress.lock().unwrap();
            if in_progress.contains(path) {
                tracing::debug!("Refresh already in progress for {}, skipping", path);
                return false;
            }
        }

        // Check if cache is fresh (within TTL) - no need to refresh
        let handle = &self.rt;
        if handle.block_on(self.dir_cache.get(path)).is_some() {
            tracing::debug!("Cache is fresh for {}, skipping refresh", path);
            return false;
        }

        // Mark as in progress
        self.refresh_in_progress
            .lock()
            .unwrap()
            .insert(path.to_string());

        // Schedule background refresh
        let webdav = self.webdav.clone();
        let cache = self.dir_cache.clone();
        let path_clone = path.to_string();
        let dav_path_str = if path == "/" {
            String::new()
        } else {
            path[1..].to_string()
        };
        let in_progress = self.refresh_in_progress.clone();

        tokio::spawn(async move {
            let etag = cache
                .get_with_etag(&path_clone)
                .await
                .and_then(|(_, etag)| etag);

            match webdav.list_dir(&dav_path_str, etag.as_deref()).await {
                // An explicit variant, not an empty Vec. The PoC could not tell
                // 304 from "the directory is now empty", and guarded the write
                // back with `!entries.is_empty()` -- so deleting the last file
                // in a directory never propagated to the cache.
                Ok(ListResult::NotModified) => {
                    tracing::debug!("{} unchanged", path_clone);
                }
                Ok(ListResult::Modified { entries, etag }) => {
                    let _ = cache.insert(path_clone.clone(), entries, etag).await;
                    tracing::debug!("Background refresh completed for {}", path_clone);
                }
                Err(e) => tracing::debug!("Background refresh of {} failed: {}", path_clone, e),
            }

            // Remove from in-progress set
            in_progress.lock().unwrap().remove(&path_clone);
        });

        true
    }

    fn get_or_create_inode(&self, path: &str) -> u64 {
        let mut path_to_inode = self.path_to_inode.lock().unwrap();

        if let Some(&ino) = path_to_inode.get(path) {
            return ino;
        }

        let mut next_inode = self.next_inode.lock().unwrap();
        let ino = *next_inode;
        *next_inode += 1;
        drop(next_inode);

        path_to_inode.insert(path.to_string(), ino);
        drop(path_to_inode);

        let mut inode_to_path = self.inode_to_path.lock().unwrap();
        inode_to_path.insert(ino, path.to_string());

        ino
    }

    fn get_path(&self, ino: u64) -> Option<String> {
        let inode_to_path = self.inode_to_path.lock().unwrap();
        inode_to_path.get(&ino).cloned()
    }

    /// Attributes for a directory entry, carrying its real mtime.
    fn attr_for(&self, ino: u64, entry: &DavEntry) -> fuser::FileAttr {
        let meta = if entry.is_dir {
            NodeMeta::dir(ino)
        } else {
            NodeMeta::file(ino, entry.size)
        };
        to_file_attr(
            &meta.with_mtime(entry.modified.map(|dt| dt.timestamp())),
            self.owner,
        )
    }
}

impl Filesystem for DavFS {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        tracing::debug!("lookup: parent={:?}, name={:?}", parent, name);

        let parent_path = match self.get_path(parent.0) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        // Build the full path
        let full_path = if parent_path == "/" {
            format!("/{}", name_str)
        } else {
            format!("{}/{}", parent_path.trim_end_matches('/'), name_str)
        };

        // Always try cache first (even stale), schedule background refresh if needed
        let entries = if let Some(cached) = self.rt.block_on(self.dir_cache.get_stale(&parent_path))
        {
            tracing::debug!("Serving lookup from cache: {}", parent_path);

            // Schedule background refresh (deduplicates automatically)
            self.schedule_refresh(&parent_path);

            cached
        } else {
            // No cache at all, must fetch synchronously (fallback)
            tracing::info!("No cache for {}, fetching synchronously", parent_path);
            let dav_path = if parent_path == "/" {
                ""
            } else {
                &parent_path[1..]
            };
            match self.rt.block_on(self.webdav.list_dir(dav_path, None)) {
                Ok(ListResult::Modified { entries, etag }) => {
                    let _ = self.rt.block_on(self.dir_cache.insert(
                        parent_path.clone(),
                        entries.clone(),
                        etag,
                    ));
                    entries
                }
                // Nothing was cached, so no validator was sent; 304 is impossible.
                Ok(ListResult::NotModified) => Vec::new(),
                Err(e) => {
                    // The typed errno matters: replying ENOENT for an
                    // unreachable server tells applications the file is gone.
                    reply.error(e.to_errno());
                    return;
                }
            }
        };

        for entry in entries {
            if entry.name == name_str {
                let ino = self.get_or_create_inode(&full_path);
                let attr = self.attr_for(ino, &entry);
                reply.entry(&TTL, &attr, Generation(0));
                return;
            }
        }
        reply.error(Errno::ENOENT);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        tracing::debug!("getattr: ino={:?}", ino);
        let ino = ino.0;

        if ino == ROOT_INO {
            reply.attr(&TTL, &to_file_attr(&NodeMeta::dir(ROOT_INO), self.owner));
            return;
        }

        let path = match self.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        // Get parent directory path
        let parent_path = if let Some(idx) = path.rfind('/') {
            if idx == 0 {
                "/"
            } else {
                &path[..idx]
            }
        } else {
            "/"
        };

        let name = path.rsplit('/').next().unwrap_or("");

        // Always try cache first (even stale), schedule background refresh if needed
        let entries = if let Some(cached) = self.rt.block_on(self.dir_cache.get_stale(parent_path))
        {
            tracing::debug!("Serving getattr from cache: {}", parent_path);

            // Schedule background refresh (deduplicates automatically)
            self.schedule_refresh(parent_path);

            cached
        } else {
            // No cache at all, must fetch synchronously (fallback)
            tracing::info!("No cache for {}, fetching synchronously", parent_path);
            let dav_path = if parent_path == "/" {
                ""
            } else {
                &parent_path[1..]
            };
            match self.rt.block_on(self.webdav.list_dir(dav_path, None)) {
                Ok(ListResult::Modified { entries, etag }) => {
                    let _ = self.rt.block_on(self.dir_cache.insert(
                        parent_path.to_string(),
                        entries.clone(),
                        etag,
                    ));
                    entries
                }
                Ok(ListResult::NotModified) => Vec::new(),
                Err(e) => {
                    // Previously this invented a 1024-byte file attribute for
                    // anything that failed to fetch, so a transient error
                    // surfaced as a real file of the wrong size.
                    reply.error(e.to_errno());
                    return;
                }
            }
        };

        for entry in entries {
            if entry.name == name {
                let attr = self.attr_for(ino, &entry);
                reply.attr(&TTL, &attr);
                return;
            }
        }
        reply.error(Errno::ENOENT);
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        tracing::debug!("readdir: ino={:?}, offset={}", ino, offset);
        let ino = ino.0;

        let dir_path = match self.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let entries = vec![
            (ino, FileType::Directory, "."),
            (ino, FileType::Directory, ".."),
        ];

        // Convert filesystem path to WebDAV path (remove leading /)
        let dav_path = if dir_path == "/" { "" } else { &dir_path[1..] };

        // Always try cache first (even stale), schedule background refresh if needed
        let dav_entries =
            if let Some(cached) = self.rt.block_on(self.dir_cache.get_stale(&dir_path)) {
                tracing::debug!("Serving readdir from cache: {}", dir_path);

                // Schedule background refresh (deduplicates automatically)
                self.schedule_refresh(&dir_path);

                cached
            } else {
                // No cache at all, must fetch synchronously (fallback)
                tracing::info!("No cache for {}, fetching synchronously", dir_path);
                match self.rt.block_on(self.webdav.list_dir(dav_path, None)) {
                    Ok(ListResult::Modified { entries, etag }) => {
                        tracing::info!(
                            "Listed {} entries at {} (etag {:?})",
                            entries.len(),
                            dav_path,
                            etag
                        );
                        let _ = self.rt.block_on(self.dir_cache.insert(
                            dir_path.clone(),
                            entries.clone(),
                            etag,
                        ));
                        entries
                    }
                    Ok(ListResult::NotModified) => Vec::new(),
                    Err(e) => {
                        tracing::error!("Failed to list {}: {}", dir_path, e);

                        // Still return . and ..
                        for (i, entry) in entries.iter().enumerate().skip(offset as usize) {
                            if reply.add(INodeNo(entry.0), (i + 1) as u64, entry.1, entry.2) {
                                break;
                            }
                        }
                        reply.ok();
                        return;
                    }
                }
            };

        let mut all_entries = entries;

        for entry in dav_entries.iter() {
            let full_path = if dir_path == "/" {
                format!("/{}", entry.name)
            } else {
                format!("{}/{}", dir_path, entry.name)
            };

            let ino = self.get_or_create_inode(&full_path);
            let kind = if entry.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            all_entries.push((ino, kind, entry.name.as_str()));
        }

        for (i, entry) in all_entries.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(entry.0), (i + 1) as u64, entry.1, entry.2) {
                break;
            }
        }

        reply.ok();
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        _size: u32,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyData,
    ) {
        tracing::debug!("read: ino={:?}, offset={}", ino, offset);

        // For PoC: Always return "no network" error when trying to read file content
        tracing::error!("Read operation not supported (PoC: no network error)");
        reply.error(Errno::ENETUNREACH);
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        tracing::debug!("open: ino={:?}", ino);
        // Allow opening files, but read will fail
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        tracing::debug!("listxattr: ino={:?}, size={}", ino, size);

        // We expose user.davfs.state xattr
        let xattr_name = "user.davfs.state";
        let total_size = xattr_name.len() + 1; // +1 for null terminator

        if size == 0 {
            // Return size needed
            reply.size(total_size as u32);
        } else if size >= total_size as u32 {
            // Return the list of xattr names
            let mut buffer = Vec::with_capacity(total_size);
            buffer.extend_from_slice(xattr_name.as_bytes());
            buffer.push(0); // null terminator
            reply.data(&buffer);
        } else {
            reply.error(Errno::ERANGE);
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        tracing::debug!("getxattr: ino={:?}, name={:?}, size={}", ino, name, size);
        let ino = ino.0;

        if name != "user.davfs.state" {
            reply.error(Errno::ENODATA);
            return;
        }

        let path = match self.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        // Determine state based on cache freshness
        let state = if ino == ROOT_INO {
            // Root is always cached
            "cached"
        } else if path.ends_with('/') {
            // It's a directory - check if we have fresh cache
            if self.rt.block_on(self.dir_cache.get(&path)).is_some() {
                "cached" // Fresh cache
            } else if self.rt.block_on(self.dir_cache.get_stale(&path)).is_some() {
                "cloud" // Stale cache, needs refresh
            } else {
                "unknown" // No cache at all
            }
        } else {
            // It's a file - check if parent directory has fresh cache
            let parent_path = if let Some(idx) = path.rfind('/') {
                if idx == 0 {
                    "/"
                } else {
                    &path[..idx]
                }
            } else {
                "/"
            };

            if self.rt.block_on(self.dir_cache.get(parent_path)).is_some() {
                // Parent has fresh cache, file metadata is known but content not downloaded
                "cloud"
            } else if self
                .rt
                .block_on(self.dir_cache.get_stale(parent_path))
                .is_some()
            {
                // Parent has stale cache, metadata might be outdated
                "cloud"
            } else {
                // Parent not cached at all
                "unknown"
            }
        };

        let value = state.as_bytes();

        if size == 0 {
            // Return size needed
            reply.size(value.len() as u32);
        } else if size >= value.len() as u32 {
            reply.data(value);
        } else {
            reply.error(Errno::ERANGE);
        }
    }
}
