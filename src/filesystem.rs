use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyXattr,
    Request,
};
use libc::ENOENT;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use crate::cache::DirectoryCache;
use crate::webdav::WebDavClient;

const TTL: Duration = Duration::from_secs(1);

const ROOT_INO: u64 = 1;

pub struct DavFS {
    webdav: WebDavClient,
    // Map inode to path
    inode_to_path: Arc<Mutex<HashMap<u64, String>>>,
    // Map path to inode
    path_to_inode: Arc<Mutex<HashMap<String, u64>>>,
    next_inode: Arc<Mutex<u64>>,
    // Directory listing cache
    dir_cache: DirectoryCache,
    // Track paths currently being refreshed to avoid duplicates
    refresh_in_progress: Arc<Mutex<HashSet<String>>>,
}

impl DavFS {
    pub fn new(webdav: WebDavClient, dir_cache: DirectoryCache) -> Self {
        let mut inode_to_path = HashMap::new();
        let path_to_inode = HashMap::new();

        // Root directory is at /
        inode_to_path.insert(ROOT_INO, String::from("/"));

        Self {
            webdav,
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
        let handle = tokio::runtime::Handle::current();
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
            if let Some((_, etag)) = cache.get_with_etag(&path_clone).await {
                if let Ok((new_entries, new_etag)) = webdav
                    .list_dir_with_etag(&dav_path_str, etag.as_deref())
                    .await
                {
                    if !new_entries.is_empty() {
                        let _ = cache
                            .insert(path_clone.clone(), new_entries, new_etag)
                            .await;
                        tracing::debug!("Background refresh completed for {}", path_clone);
                    }
                }
            }

            // Remove from in-progress set
            in_progress.lock().unwrap().remove(&path_clone);
        });

        true
    }

    /* Disabled: Too aggressive, initial prefetch + on-demand refresh is sufficient
    fn prefetch_subdirectories(&self, dir_path: &str, entries: &[crate::webdav::DavEntry]) {
        // Background prefetch of subdirectories for faster navigation
        // Go 3 levels deep for rapid prefetching
        let subdirs: Vec<_> = entries.iter()
            .filter(|e| e.is_dir)
            .map(|e| {
                if dir_path == "/" {
                    format!("/{}", e.name)
                } else {
                    format!("{}/{}", dir_path, e.name)
                }
            })
            .collect();

        if subdirs.is_empty() {
            return;
        }

        let webdav = self.webdav.clone();
        let cache = self.dir_cache.clone();

        // Spawn background task to prefetch recursively
        std::thread::spawn(move || {
            // Create a new runtime in the background thread
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            fn prefetch_recursive(
                rt: &tokio::runtime::Runtime,
                webdav: &crate::webdav::WebDavClient,
                cache: &crate::cache::DirectoryCache,
                path: &str,
                depth: u32,
                max_depth: u32,
            ) {
                if depth >= max_depth {
                    return;
                }

                // Check if already cached and get etag for conditional request
                let cached_etag = rt.block_on(cache.get_with_etag(path)).and_then(|(_, etag)| etag);

                // Fetch directory listing with conditional request
                let dav_path = if path == "/" { "" } else { &path[1..] };
                match rt.block_on(webdav.list_dir_with_etag(dav_path, cached_etag.as_deref())) {
                    Ok((entries, etag)) => {
                        // If we got HTTP 304, entries will be empty - use cached data
                        if entries.is_empty() && cached_etag.is_some() {
                            tracing::info!("Directory {} not modified (HTTP 304), using cached data", path);
                            // Still recurse into subdirectories
                            if let Some((cached_entries, _)) = rt.block_on(cache.get_with_etag(path)) {
                                let subdirs: Vec<_> = cached_entries.iter()
                                    .filter(|e| e.is_dir)
                                    .map(|e| {
                                        if path == "/" {
                                            format!("/{}", e.name)
                                        } else {
                                            format!("{}/{}", path, e.name)
                                        }
                                    })
                                    .collect();

                                for subdir in subdirs {
                                    prefetch_recursive(rt, webdav, cache, &subdir, depth + 1, max_depth);
                                }
                            }
                            return;
                        }

                        let num_entries = entries.len();

                        // Find subdirectories before we move entries
                        let subdirs: Vec<_> = entries.iter()
                            .filter(|e| e.is_dir)
                            .map(|e| {
                                if path == "/" {
                                    format!("/{}", e.name)
                                } else {
                                    format!("{}/{}", path, e.name)
                                }
                            })
                            .collect();

                        // Cache this directory with etag
                        let _ = rt.block_on(cache.insert(path.to_string(), entries, etag.clone()));

                        tracing::info!("Prefetched {} (depth {}, {} entries, {} subdirs, etag: {:?})",
                                      path, depth, num_entries, subdirs.len(), etag);

                        // Recursively prefetch subdirectories
                        for subdir in subdirs {
                            prefetch_recursive(rt, webdav, cache, &subdir, depth + 1, max_depth);
                        }
                    }
                    Err(e) => {
                        tracing::debug!("Failed to prefetch {}: {}", path, e);
                    }
                }
            }

            // Prefetch up to 4 levels deep for very aggressive caching
            for subdir_path in subdirs {
                prefetch_recursive(&rt, &webdav, &cache, &subdir_path, 1, 4);
            }
        });
    }
    */

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

    fn root_attr() -> FileAttr {
        FileAttr {
            ino: ROOT_INO,
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    fn dir_attr(ino: u64) -> FileAttr {
        FileAttr {
            ino,
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    fn file_attr(ino: u64, size: u64) -> FileAttr {
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }
}

impl Filesystem for DavFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        tracing::debug!("lookup: parent={}, name={:?}", parent, name);

        let parent_path = match self.get_path(parent) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(ENOENT);
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
        let entries = if let Some(cached) =
            tokio::runtime::Handle::current().block_on(self.dir_cache.get_stale(&parent_path))
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
            match tokio::runtime::Handle::current()
                .block_on(self.webdav.list_dir_with_etag(dav_path, None))
            {
                Ok((entries, etag)) => {
                    let _ = tokio::runtime::Handle::current().block_on(self.dir_cache.insert(
                        parent_path.clone(),
                        entries.clone(),
                        etag,
                    ));
                    entries
                }
                Err(_) => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        for entry in entries {
            if entry.name == name_str {
                let ino = self.get_or_create_inode(&full_path);
                let attr = if entry.is_dir {
                    Self::dir_attr(ino)
                } else {
                    Self::file_attr(ino, entry.size)
                };
                reply.entry(&TTL, &attr, 0);
                return;
            }
        }
        reply.error(ENOENT);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        tracing::debug!("getattr: ino={}", ino);

        if ino == ROOT_INO {
            reply.attr(&TTL, &Self::root_attr());
            return;
        }

        let path = match self.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
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
        let entries = if let Some(cached) =
            tokio::runtime::Handle::current().block_on(self.dir_cache.get_stale(parent_path))
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
            match tokio::runtime::Handle::current()
                .block_on(self.webdav.list_dir_with_etag(dav_path, None))
            {
                Ok((entries, etag)) => {
                    let _ = tokio::runtime::Handle::current().block_on(self.dir_cache.insert(
                        parent_path.to_string(),
                        entries.clone(),
                        etag,
                    ));
                    entries
                }
                Err(_) => {
                    // Fallback to generic file attributes
                    let attr = Self::file_attr(ino, 1024);
                    reply.attr(&TTL, &attr);
                    return;
                }
            }
        };

        for entry in entries {
            if entry.name == name {
                let attr = if entry.is_dir {
                    Self::dir_attr(ino)
                } else {
                    Self::file_attr(ino, entry.size)
                };
                reply.attr(&TTL, &attr);
                return;
            }
        }
        reply.error(ENOENT);
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        tracing::debug!("readdir: ino={}, offset={}", ino, offset);

        let dir_path = match self.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
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
        let dav_entries = if let Some(cached) =
            tokio::runtime::Handle::current().block_on(self.dir_cache.get_stale(&dir_path))
        {
            tracing::debug!("Serving readdir from cache: {}", dir_path);

            // Schedule background refresh (deduplicates automatically)
            self.schedule_refresh(&dir_path);

            cached
        } else {
            // No cache at all, must fetch synchronously (fallback)
            tracing::info!("No cache for {}, fetching synchronously", dir_path);
            match tokio::runtime::Handle::current()
                .block_on(self.webdav.list_dir_with_etag(dav_path, None))
            {
                Ok((entries, etag)) => {
                    tracing::info!(
                        "Listed {} entries from WebDAV at path {} with etag: {:?}",
                        entries.len(),
                        dav_path,
                        etag
                    );
                    let _ = tokio::runtime::Handle::current().block_on(self.dir_cache.insert(
                        dir_path.clone(),
                        entries.clone(),
                        etag,
                    ));

                    // Note: prefetch_subdirectories disabled - initial prefetch + on-demand refresh is sufficient

                    entries
                }
                Err(e) => {
                    tracing::error!("Failed to list directory: {}", e);

                    // Still return . and ..
                    for (i, entry) in entries.iter().enumerate().skip(offset as usize) {
                        if reply.add(entry.0, (i + 1) as i64, entry.1, entry.2) {
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
            if reply.add(entry.0, (i + 1) as i64, entry.1, entry.2) {
                break;
            }
        }

        reply.ok();
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        _size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        tracing::debug!("read: ino={}, offset={}", ino, offset);

        // For PoC: Always return "no network" error when trying to read file content
        tracing::error!("Read operation not supported (PoC: no network error)");
        reply.error(libc::ENETUNREACH); // Network unreachable
    }

    fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        tracing::debug!("open: ino={}", ino);
        // Allow opening files, but read will fail
        reply.opened(0, 0);
    }

    fn listxattr(&mut self, _req: &Request, ino: u64, size: u32, reply: ReplyXattr) {
        tracing::debug!("listxattr: ino={}, size={}", ino, size);

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
            reply.error(libc::ERANGE);
        }
    }

    fn getxattr(&mut self, _req: &Request, ino: u64, name: &OsStr, size: u32, reply: ReplyXattr) {
        tracing::debug!("getxattr: ino={}, name={:?}, size={}", ino, name, size);

        if name != "user.davfs.state" {
            reply.error(libc::ENODATA);
            return;
        }

        let path = match self.get_path(ino) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        // Determine state based on cache freshness
        let state = if ino == ROOT_INO {
            // Root is always cached
            "cached"
        } else if path.ends_with('/') {
            // It's a directory - check if we have fresh cache
            if tokio::runtime::Handle::current()
                .block_on(self.dir_cache.get(&path))
                .is_some()
            {
                "cached" // Fresh cache
            } else if tokio::runtime::Handle::current()
                .block_on(self.dir_cache.get_stale(&path))
                .is_some()
            {
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

            if tokio::runtime::Handle::current()
                .block_on(self.dir_cache.get(parent_path))
                .is_some()
            {
                // Parent has fresh cache, file metadata is known but content not downloaded
                "cloud"
            } else if tokio::runtime::Handle::current()
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
            reply.error(libc::ERANGE);
        }
    }
}
