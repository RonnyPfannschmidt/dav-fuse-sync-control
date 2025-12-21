use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, Request,
};
use libc::ENOENT;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use crate::webdav::WebDavClient;

const TTL: Duration = Duration::from_secs(1);

const ROOT_INO: u64 = 1;

pub struct DavFS {
    webdav: WebDavClient,
    runtime: tokio::runtime::Runtime,
    // Map inode to path
    inode_to_path: Arc<Mutex<HashMap<u64, String>>>,
    // Map path to inode
    path_to_inode: Arc<Mutex<HashMap<String, u64>>>,
    next_inode: Arc<Mutex<u64>>,
}

impl DavFS {
    pub fn new(webdav: WebDavClient) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut inode_to_path = HashMap::new();
        let mut path_to_inode = HashMap::new();
        
        // Root directory is at /
        inode_to_path.insert(ROOT_INO, String::from("/"));
        path_to_inode.insert(String::from("/"), ROOT_INO);
        
        Self {
            webdav,
            runtime,
            inode_to_path: Arc::new(Mutex::new(inode_to_path)),
            path_to_inode: Arc::new(Mutex::new(path_to_inode)),
            next_inode: Arc::new(Mutex::new(2)),
        }
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
            blocks: (size + 511) / 512,
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
        
        // Try to list parent directory to find this entry
        let dav_path = if parent_path == "/" { "" } else { &parent_path[1..] };
        match self.runtime.block_on(self.webdav.list_dir(dav_path)) {
            Ok(entries) => {
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
            Err(_) => {
                reply.error(ENOENT);
            }
        }
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
        
        // List parent to find this entry
        let dav_path = if parent_path == "/" { "" } else { &parent_path[1..] };
        match self.runtime.block_on(self.webdav.list_dir(dav_path)) {
            Ok(entries) => {
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
            Err(_) => {
                // Fallback to generic file attributes
                let attr = Self::file_attr(ino, 1024);
                reply.attr(&TTL, &attr);
            }
        }
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
        let dav_path = if dir_path == "/" {
            ""
        } else {
            &dir_path[1..]
        };
        
        // Try to list from WebDAV
        match self.runtime.block_on(self.webdav.list_dir(dav_path)) {
            Ok(dav_entries) => {
                tracing::info!("Listed {} entries from WebDAV at path {}", dav_entries.len(), dav_path);
                
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
            }
            Err(e) => {
                tracing::error!("Failed to list directory: {}", e);
                
                // Still return . and ..
                for (i, entry) in entries.iter().enumerate().skip(offset as usize) {
                    if reply.add(entry.0, (i + 1) as i64, entry.1, entry.2) {
                        break;
                    }
                }
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
}
