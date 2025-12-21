# davfs-sync: Rust Implementation Notes

## Why Rust?

### Performance Benefits
- **Zero-cost abstractions**: No runtime overhead for high-level features
- **No GC pauses**: Predictable latency for FUSE operations
- **Efficient memory**: Lower per-file metadata overhead
- **Concurrent by default**: Tokio async runtime for network I/O
- **Type safety**: Catch bugs at compile time

### Key Libraries

```toml
[dependencies]
# FUSE - mature, production-ready
fuser = "0.14"

# Async runtime - industry standard
tokio = { version = "1", features = ["full"] }

# HTTP/WebDAV - reliable, well-maintained
reqwest = { version = "0.11", features = ["json"] }

# Database - bundled SQLite, no system deps
rusqlite = { version = "0.31", features = ["bundled"] }

# Desktop integration
secret-service = "3.0"  # GNOME Keyring, KWallet
gio = "0.18"            # GSettings (optional)
```

---

## FUSE Implementation Strategy

### Async + FUSE Challenge

FUSE operations are synchronous, but WebDAV requires async I/O. Solution:

```rust
use fuser::{Filesystem, Request, ReplyAttr};
use tokio::runtime::Runtime;

pub struct DavFS {
    rt: Runtime,
    webdav: Arc<WebDavClient>,
    cache: Arc<Cache>,
    metadata: Arc<MetadataStore>,
}

impl Filesystem for DavFS {
    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        // Block on async operation
        let result = self.rt.block_on(async {
            self.metadata.get_attr(ino).await
        });
        
        match result {
            Ok(attr) => reply.attr(&Duration::from_secs(1), &attr),
            Err(e) => reply.error(libc::EIO),
        }
    }
    
    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: fuser::ReplyData,
    ) {
        let result = self.rt.block_on(async {
            // Try cache first
            if let Some(data) = self.cache.read(ino, offset, size).await? {
                return Ok(data);
            }
            
            // Download from WebDAV
            self.webdav.download_range(ino, offset, size).await
        });
        
        match result {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(libc::EIO),
        }
    }
}
```

### Background Sync Worker

```rust
use tokio::sync::mpsc;

pub enum SyncCommand {
    Pin { path: PathBuf },
    Download { path: PathBuf },
    Free { path: PathBuf },
}

pub struct SyncWorker {
    rx: mpsc::UnboundedReceiver<SyncCommand>,
    webdav: Arc<WebDavClient>,
    cache: Arc<Cache>,
}

impl SyncWorker {
    pub async fn run(mut self) {
        while let Some(cmd) = self.rx.recv().await {
            match cmd {
                SyncCommand::Pin { path } => {
                    self.handle_pin(&path).await;
                }
                SyncCommand::Download { path } => {
                    self.handle_download(&path).await;
                }
                SyncCommand::Free { path } => {
                    self.handle_free(&path).await;
                }
            }
        }
    }
    
    async fn handle_download(&self, path: &Path) -> Result<()> {
        let ino = self.cache.path_to_ino(path)?;
        
        // Stream from WebDAV to cache
        let mut stream = self.webdav.download_stream(ino).await?;
        let mut file = self.cache.create_cache_file(ino).await?;
        
        while let Some(chunk) = stream.next().await {
            file.write_all(&chunk?).await?;
            
            // Update progress xattr
            self.cache.update_progress(ino, file.metadata().await?.len()).await?;
        }
        
        Ok(())
    }
}
```

---

## WebDAV Client

```rust
use reqwest::Client;
use url::Url;

pub struct WebDavClient {
    client: Client,
    base_url: Url,
    username: String,
    password: String,
}

impl WebDavClient {
    pub async fn list_dir(&self, path: &str) -> Result<Vec<DavEntry>> {
        let url = self.base_url.join(path)?;
        
        let response = self.client
            .request(Method::from_bytes(b"PROPFIND")?, url)
            .basic_auth(&self.username, Some(&self.password))
            .header("Depth", "1")
            .body(r#"<?xml version="1.0"?>
                <d:propfind xmlns:d="DAV:">
                  <d:prop>
                    <d:displayname/>
                    <d:getcontentlength/>
                    <d:getlastmodified/>
                    <d:getetag/>
                    <d:resourcetype/>
                  </d:prop>
                </d:propfind>"#)
            .send()
            .await?;
        
        let body = response.text().await?;
        self.parse_propfind_response(&body)
    }
    
    pub async fn download_range(
        &self,
        path: &str,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>> {
        let url = self.base_url.join(path)?;
        
        let range = format!("bytes={}-{}", offset, offset + length - 1);
        
        let response = self.client
            .get(url)
            .basic_auth(&self.username, Some(&self.password))
            .header("Range", range)
            .send()
            .await?;
        
        Ok(response.bytes().await?.to_vec())
    }
}
```

---

## Extended Attributes

```rust
use std::ffi::OsStr;

impl Filesystem for DavFS {
    fn getxattr(
        &mut self,
        _req: &Request,
        ino: u64,
        name: &OsStr,
        size: u32,
        reply: fuser::ReplyXattr,
    ) {
        let name_str = name.to_str().unwrap_or("");
        
        if !name_str.starts_with("user.davfs.") {
            reply.error(libc::ENODATA);
            return;
        }
        
        let result = self.rt.block_on(async {
            match name_str {
                "user.davfs.status" => {
                    let status = self.metadata.get_status(ino).await?;
                    Ok(status.as_bytes().to_vec())
                }
                "user.davfs.progress" => {
                    let progress = self.metadata.get_progress(ino).await?;
                    Ok(format!("{:.2}", progress).into_bytes())
                }
                "user.davfs.pinned" => {
                    let pinned = self.metadata.is_pinned(ino).await?;
                    Ok(if pinned { b"1".to_vec() } else { b"0".to_vec() })
                }
                _ => Err(anyhow::anyhow!("Unknown attribute")),
            }
        });
        
        match result {
            Ok(data) => {
                if size == 0 {
                    reply.size(data.len() as u32);
                } else if size < data.len() as u32 {
                    reply.error(libc::ERANGE);
                } else {
                    reply.data(&data);
                }
            }
            Err(_) => reply.error(libc::ENODATA),
        }
    }
    
    fn setxattr(
        &mut self,
        _req: &Request,
        ino: u64,
        name: &OsStr,
        value: &[u8],
        _flags: i32,
        _position: u32,
        reply: fuser::ReplyEmpty,
    ) {
        let name_str = name.to_str().unwrap_or("");
        
        // Action triggers (write-only)
        match name_str {
            "user.davfs.do_pin" => {
                self.sync_tx.send(SyncCommand::Pin { ino }).ok();
                reply.ok();
            }
            "user.davfs.do_download" => {
                self.sync_tx.send(SyncCommand::Download { ino }).ok();
                reply.ok();
            }
            "user.davfs.do_free" => {
                self.sync_tx.send(SyncCommand::Free { ino }).ok();
                reply.ok();
            }
            _ => reply.error(libc::ENOTSUP),
        }
    }
}
```

---

## Performance Optimizations

### 1. Inode Cache
```rust
use lru::LruCache;

pub struct InodeCache {
    cache: Mutex<LruCache<u64, FileAttr>>,
}

impl InodeCache {
    pub fn get(&self, ino: u64) -> Option<FileAttr> {
        self.cache.lock().unwrap().get(&ino).copied()
    }
    
    pub fn put(&self, ino: u64, attr: FileAttr) {
        self.cache.lock().unwrap().put(ino, attr);
    }
}
```

### 2. Parallel Operations
```rust
use futures::stream::{self, StreamExt};

pub async fn download_directory(&self, path: &Path) -> Result<()> {
    let entries = self.webdav.list_dir(path).await?;
    
    // Download files in parallel (max 3 concurrent)
    stream::iter(entries)
        .map(|entry| self.download_file(&entry))
        .buffer_unordered(3)
        .collect::<Vec<_>>()
        .await;
    
    Ok(())
}
```

### 3. Zero-Copy Where Possible
```rust
use std::os::unix::fs::FileExt;

// Read directly into FUSE buffer
pub fn read_cached(&self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
    let file = self.cache_file(ino)?;
    file.read_at(buf, offset).map_err(Into::into)
}
```

---

## Testing Strategy

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_webdav_list_dir() {
        let mock_server = MockWebDavServer::start();
        let client = WebDavClient::new(mock_server.url(), "user", "pass");
        
        let entries = client.list_dir("/").await.unwrap();
        assert_eq!(entries.len(), 2);
    }
    
    #[test]
    fn test_fuse_getattr() {
        let mut fs = create_test_fs();
        let reply = TestReply::new();
        
        fs.getattr(&Request::default(), 1, reply);
        
        assert!(reply.has_attr());
    }
}
```

---

## Migration Path from Python Prototype

1. **Phase 1**: Port core FUSE operations to Rust
2. **Phase 2**: Port WebDAV client with reqwest
3. **Phase 3**: Port metadata store with rusqlite
4. **Phase 4**: Port CLI with clap
5. **Phase 5**: Add desktop integration (GSettings, Secret Service)

Can keep Python for prototyping file manager plugins, then port if needed.
