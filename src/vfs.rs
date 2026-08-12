//! The filesystem core.
//!
//! Async, `Send + Sync`, and entirely independent of FUSE: every FUSE handler
//! is a shim that copies its arguments and calls a method here. That is what
//! makes the interesting behaviour testable with `#[tokio::test]` against a
//! `MockRemote` and a tempfile database -- no mount, no root, no `/dev/fuse`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{BoxFuture, Shared};
use futures::FutureExt;
use tokio::sync::Semaphore;

use crate::error::RemoteError;
use crate::fuse::attr::{to_file_attr, NodeMeta, OwnerIds};
use crate::remote::{Listing, NodeId, NodeKind, RemoteStorage, SizeHint};
use crate::store::meta::{single_display_name, CachedNode};
use crate::store::{ContentCache, MetaStore};

/// What `open()` does when a file's content is not cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnOpenUncached {
    /// Fail immediately without fetching. This is the PoC's behaviour with a
    /// config knob on it, and makes the filesystem unable to read anything.
    Error,
    /// Download, then return. The failure lands at `open()`, which is where
    /// every application is prepared to handle one.
    #[default]
    Block,
    /// Return a handle at once; the first `read()` awaits the same download.
    Background,
}

#[derive(Debug, Clone)]
pub struct VfsConfig {
    /// How long a cached directory listing is considered fresh.
    pub ttl: Duration,
    /// Attribute lifetime handed to the kernel.
    pub attr_ttl: Duration,
    pub on_open_uncached: OnOpenUncached,
    /// Concurrent *network* operations. A warm cache is unaffected.
    pub max_concurrent: usize,
    /// Total concurrent FUSE operations, so a pathological client cannot
    /// exhaust memory.
    pub max_inflight: usize,
    pub open_timeout: Duration,
    pub read_only: bool,
}

impl Default for VfsConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(30),
            attr_ttl: Duration::from_secs(30),
            on_open_uncached: OnOpenUncached::Block,
            max_concurrent: 4,
            max_inflight: 256,
            open_timeout: Duration::from_secs(30),
            read_only: true,
        }
    }
}

/// An open file handle: a blob descriptor opened once, at `open()`.
///
/// Holding the descriptor is what lets eviction unlink a blob while readers are
/// active -- Linux keeps the inode alive until the last handle closes.
struct OpenFile {
    node: NodeId,
    file: Option<Arc<std::fs::File>>,
    /// Set when the handle was opened in `Background` mode and the download is
    /// still running; the first `read` awaits it.
    pending: Option<DownloadHandle>,
}

type DownloadHandle = Shared<BoxFuture<'static, Result<(String, u64), Arc<RemoteError>>>>;

pub struct Vfs {
    remote: Arc<dyn RemoteStorage>,
    meta: MetaStore,
    content: ContentCache,
    owner: OwnerIds,
    config: VfsConfig,

    remote_permits: Arc<Semaphore>,
    inflight: Arc<Semaphore>,

    open_files: Mutex<HashMap<u64, Arc<OpenFile>>>,
    next_fh: AtomicU64,

    /// One download per node, however many callers ask for it.
    downloads: Mutex<HashMap<NodeId, DownloadHandle>>,
    /// Directories with a background revalidation already running.
    refreshing: Mutex<HashSet<NodeId>>,
}

impl Vfs {
    pub async fn new(
        remote: Arc<dyn RemoteStorage>,
        meta: MetaStore,
        content: ContentCache,
        owner: OwnerIds,
        config: VfsConfig,
    ) -> Result<Arc<Self>, RemoteError> {
        // Before anything can start writing blobs.
        if let Ok(n) = content.sweep_partials() {
            if n > 0 {
                tracing::info!("removed {} interrupted downloads", n);
            }
        }

        Ok(Arc::new(Self {
            remote_permits: Arc::new(Semaphore::new(config.max_concurrent)),
            inflight: Arc::new(Semaphore::new(config.max_inflight)),
            open_files: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            downloads: Mutex::new(HashMap::new()),
            refreshing: Mutex::new(HashSet::new()),
            remote,
            meta,
            content,
            owner,
            config,
        }))
    }

    pub fn config(&self) -> &VfsConfig {
        &self.config
    }

    pub fn attr_ttl(&self) -> Duration {
        self.config.attr_ttl
    }

    /// Admission control. Callers reply `EAGAIN` when this is unavailable,
    /// rather than queueing without bound.
    pub fn try_admit(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.inflight.clone().try_acquire_owned().ok()
    }

    // -- helpers ----------------------------------------------------------

    async fn node_of(&self, ino: u64) -> Result<CachedNode, RemoteError> {
        let id = self
            .meta
            .node_for_ino(ino)
            .await
            .ok_or(RemoteError::NotFound)?;
        self.meta.node(&id).await.ok_or(RemoteError::NotFound)
    }

    async fn attr_of(&self, node: &CachedNode) -> Result<fuser::FileAttr, RemoteError> {
        let ino = self.meta.ino_for(&node.id).await.map_err(io_err)?;
        // A cached blob knows the true length even when the server would not
        // say -- which is how an exported Google doc stops reporting a guess.
        let size = node
            .content_len
            .filter(|_| node.size == SizeHint::Unknown)
            .unwrap_or_else(|| node.size.or_placeholder());

        Ok(to_file_attr(
            &NodeMeta {
                ino,
                is_dir: node.is_dir(),
                size,
                mtime: node.mtime,
            },
            self.owner,
        ))
    }

    /// Serve a directory's children, refreshing in the background if stale.
    ///
    /// Cache first, always: a slow or unreachable server never blocks a
    /// listing that has been seen before.
    async fn children_of(self: &Arc<Self>, dir: &NodeId) -> Result<Vec<CachedNode>, RemoteError> {
        if let Some(listing) = self.meta.children(dir).await {
            if listing.age.is_some() {
                if !listing.is_fresh(self.config.ttl) {
                    self.spawn_refresh(dir.clone());
                }
                return Ok(listing.children);
            }
        }
        // Never listed: this one has to wait.
        self.refresh_now(dir).await?;
        Ok(self
            .meta
            .children(dir)
            .await
            .map(|l| l.children)
            .unwrap_or_default())
    }

    /// Fetch a listing and write it to the cache.
    async fn refresh_now(&self, dir: &NodeId) -> Result<(), RemoteError> {
        let validator = self.meta.children(dir).await.and_then(|l| l.validator);

        let _permit = self
            .remote_permits
            .acquire()
            .await
            .map_err(|_| RemoteError::Protocol("permit semaphore closed".into()))?;
        let listing = self.remote.list(dir, validator.as_ref()).await?;

        match &listing {
            Listing::Unchanged => self.meta.touch_listing(dir).await.map_err(io_err)?,
            Listing::Fresh { .. } => self
                .meta
                .upsert_listing(dir, &listing)
                .await
                .map_err(io_err)?,
        }
        Ok(())
    }

    /// Revalidate in the background, at most once per directory at a time.
    fn spawn_refresh(self: &Arc<Self>, dir: NodeId) {
        {
            let mut in_flight = self.refreshing.lock().unwrap();
            if !in_flight.insert(dir.clone()) {
                return;
            }
        }
        let this = self.clone();
        tokio::spawn(async move {
            if let Err(e) = this.refresh_now(&dir).await {
                tracing::debug!("background refresh of {} failed: {}", dir, e);
            }
            this.refreshing.lock().unwrap().remove(&dir);
        });
    }

    // -- metadata operations ----------------------------------------------

    pub async fn lookup(
        self: &Arc<Self>,
        parent_ino: u64,
        name: &str,
    ) -> Result<fuser::FileAttr, RemoteError> {
        let parent = self.node_of(parent_ino).await?;
        if !parent.is_dir() {
            return Err(RemoteError::NotFound);
        }

        // Serve from cache when the listing is fresh enough.
        if let Some(hit) = self.meta.lookup_child(&parent.id, name).await {
            let listing_fresh = self
                .meta
                .children(&parent.id)
                .await
                .map(|l| l.is_fresh(self.config.ttl))
                .unwrap_or(false);
            if !listing_fresh {
                self.spawn_refresh(parent.id.clone());
            }
            return self.attr_of(&hit).await;
        }

        // Not cached. Ask the backend directly when it can resolve a single
        // name cheaply; otherwise fall back to listing the parent.
        match self.remote.lookup(&parent.id, name).await {
            Ok(entry) => {
                let display = single_display_name(&entry);
                self.meta
                    .upsert_node(&entry, &display)
                    .await
                    .map_err(io_err)?;
                let node = self
                    .meta
                    .node(&entry.id)
                    .await
                    .ok_or(RemoteError::NotFound)?;
                self.attr_of(&node).await
            }
            Err(RemoteError::NotFound) => {
                // The backend may simply not have this name; make sure our
                // listing is current before believing it.
                self.refresh_now(&parent.id).await?;
                let node = self
                    .meta
                    .lookup_child(&parent.id, name)
                    .await
                    .ok_or(RemoteError::NotFound)?;
                self.attr_of(&node).await
            }
            Err(e) => Err(e),
        }
    }

    pub async fn getattr(self: &Arc<Self>, ino: u64) -> Result<fuser::FileAttr, RemoteError> {
        let node = self.node_of(ino).await?;
        self.attr_of(&node).await
    }

    /// Directory entries, including `.` and `..`.
    pub async fn readdir(
        self: &Arc<Self>,
        ino: u64,
    ) -> Result<Vec<(u64, fuser::FileType, String)>, RemoteError> {
        let dir = self.node_of(ino).await?;
        if !dir.is_dir() {
            return Err(RemoteError::Protocol("not a directory".into()));
        }

        let parent_ino = match &dir.parent {
            Some(p) => self.meta.ino_for(p).await.map_err(io_err)?,
            None => ino,
        };

        let mut out = vec![
            (ino, fuser::FileType::Directory, ".".to_string()),
            (parent_ino, fuser::FileType::Directory, "..".to_string()),
        ];

        for child in self.children_of(&dir.id).await? {
            let child_ino = self.meta.ino_for(&child.id).await.map_err(io_err)?;
            out.push((
                child_ino,
                if child.is_dir() {
                    fuser::FileType::Directory
                } else {
                    fuser::FileType::RegularFile
                },
                child.display_name,
            ));
        }
        Ok(out)
    }

    /// `readdirplus`: the same listing plus attributes, so the kernel does not
    /// follow every `ls` with a storm of individual lookups.
    pub async fn readdirplus(
        self: &Arc<Self>,
        ino: u64,
    ) -> Result<Vec<(u64, fuser::FileType, String, fuser::FileAttr)>, RemoteError> {
        let mut out = Vec::new();
        let dir = self.node_of(ino).await?;

        for child in self.children_of(&dir.id).await? {
            let attr = self.attr_of(&child).await?;
            out.push((
                attr.ino.0,
                if child.is_dir() {
                    fuser::FileType::Directory
                } else {
                    fuser::FileType::RegularFile
                },
                child.display_name,
                attr,
            ));
        }
        Ok(out)
    }

    // -- content ----------------------------------------------------------

    /// Ensure a node's content is in the blob cache, returning its key and size.
    ///
    /// Single-flight: twenty concurrent opens of one file produce one GET.
    /// This generalises the PoC's `refresh_in_progress` boolean into a handle
    /// that others can actually join.
    fn download(self: &Arc<Self>, node: &CachedNode) -> Result<DownloadHandle, RemoteError> {
        let key = ContentCache::key_for(&node.id, node.validator.as_ref()).ok_or_else(|| {
            RemoteError::Protocol(format!(
                "{} has no validator, so its content cannot be cached safely",
                node.id
            ))
        })?;

        let mut in_flight = self.downloads.lock().unwrap();
        if let Some(existing) = in_flight.get(&node.id) {
            return Ok(existing.clone());
        }

        let this = self.clone();
        let id = node.id.clone();
        let validator = node.validator.clone();
        let key_for_task = key.clone();

        let fut = async move {
            let result: Result<(String, u64), RemoteError> = async {
                if this.content.has(&key_for_task) {
                    let len = this.content.len_of(&key_for_task).unwrap_or(0);
                    return Ok((key_for_task.clone(), len));
                }

                let _permit = this
                    .remote_permits
                    .acquire()
                    .await
                    .map_err(|_| RemoteError::Protocol("permit semaphore closed".into()))?;

                // Whole-file download. One ranged GET per 128 KiB FUSE read is
                // 30-100x slower over a WAN and invites rate limiting; ranges
                // are for resume and opt-in streaming only.
                let stream = this.remote.open_read(&id, None, validator.as_ref()).await?;
                let len = this.content.store(&key_for_task, stream.body).await?;

                // Only now does the database point at the blob: it is on disk,
                // fsync'd and renamed into place.
                this.meta
                    .set_content(&id, &key_for_task, len, validator.as_ref())
                    .await
                    .map_err(io_err)?;
                Ok((key_for_task.clone(), len))
            }
            .await;

            this.downloads.lock().unwrap().remove(&id);
            result.map_err(Arc::new)
        };

        let handle: DownloadHandle = fut.boxed().shared();
        in_flight.insert(node.id.clone(), handle.clone());
        Ok(handle)
    }

    pub async fn open(self: &Arc<Self>, ino: u64, write: bool) -> Result<u64, RemoteError> {
        if write && self.config.read_only {
            return Err(RemoteError::Unsupported("write"));
        }

        let node = self.node_of(ino).await?;
        if node.is_dir() {
            return Err(RemoteError::Protocol("is a directory".into()));
        }
        self.meta.touch_access(&node.id).await.map_err(io_err)?;

        // Is the cached blob still the current version?
        //
        // The key is derived from (identity, validator), so a changed file has
        // a different key. Trusting the *stored* content_key instead of
        // recomputing it is how stale bytes get served: the row still points at
        // the blob fetched for the previous validator.
        let wanted_key = ContentCache::key_for(&node.id, node.validator.as_ref());

        if let Some(stored) = node.content_key.as_deref() {
            let current = wanted_key.as_deref() == Some(stored);
            if current && self.content.has(stored) {
                let file = self.content.open(stored).map_err(RemoteError::Io)?;
                return Ok(self.register(OpenFile {
                    node: node.id.clone(),
                    file: Some(Arc::new(file)),
                    pending: None,
                }));
            }

            if current {
                tracing::warn!("blob {} missing for {}; refetching", stored, node.id);
            } else {
                tracing::debug!("{} changed; discarding superseded blob {}", node.id, stored);
            }
            // Database first, then the file: a row must never point at a blob
            // that is not there.
            if let Some(old) = self.meta.clear_content(&node.id).await.map_err(io_err)? {
                if !current {
                    let _ = self.content.remove(&old);
                }
            }
        }

        match self.config.on_open_uncached {
            OnOpenUncached::Error => Err(RemoteError::Unsupported("content not cached")),

            OnOpenUncached::Block => {
                let handle = self.download(&node)?;
                let (key, _) = tokio::time::timeout(self.config.open_timeout, handle)
                    .await
                    .map_err(|_| RemoteError::Offline("download timed out".into()))?
                    .map_err(unwrap_arc)?;
                let file = self.content.open(&key).map_err(RemoteError::Io)?;
                Ok(self.register(OpenFile {
                    node: node.id.clone(),
                    file: Some(Arc::new(file)),
                    pending: None,
                }))
            }

            OnOpenUncached::Background => {
                let handle = self.download(&node)?;
                Ok(self.register(OpenFile {
                    node: node.id.clone(),
                    file: None,
                    pending: Some(handle),
                }))
            }
        }
    }

    fn register(&self, open: OpenFile) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        self.open_files.lock().unwrap().insert(fh, Arc::new(open));
        fh
    }

    pub async fn read(
        self: &Arc<Self>,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, RemoteError> {
        let open = self
            .open_files
            .lock()
            .unwrap()
            .get(&fh)
            .cloned()
            .ok_or(RemoteError::NotFound)?;

        // Background mode: the first read joins the download.
        let file = match &open.file {
            Some(f) => f.clone(),
            None => {
                let pending = open
                    .pending
                    .clone()
                    .ok_or_else(|| RemoteError::Protocol("handle has no content".into()))?;
                let (key, _) = pending.await.map_err(unwrap_arc)?;
                let f = Arc::new(self.content.open(&key).map_err(RemoteError::Io)?);
                // Replace the handle so later reads skip straight to the fd.
                self.open_files.lock().unwrap().insert(
                    fh,
                    Arc::new(OpenFile {
                        node: open.node.clone(),
                        file: Some(f.clone()),
                        pending: None,
                    }),
                );
                f
            }
        };

        // One positional read on a blocking thread. No network, no lock.
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::FileExt;
            let mut buf = vec![0u8; size as usize];
            let n = file.read_at(&mut buf, offset)?;
            buf.truncate(n);
            Ok::<_, std::io::Error>(buf)
        })
        .await
        .map_err(|e| RemoteError::Protocol(format!("read task failed: {e}")))?
        .map_err(RemoteError::Io)
    }

    pub fn release(&self, fh: u64) {
        self.open_files.lock().unwrap().remove(&fh);
    }

    // -- extended attributes ----------------------------------------------

    /// Attributes exposed for every node.
    ///
    /// `user.davfs.state` keeps its exact `cached`/`cloud`/`unknown` vocabulary
    /// because the Nautilus extension depends on it.
    pub const XATTR_NAMES: &'static [&'static str] = &[
        "user.davfs.state",
        "user.davfs.status",
        "user.davfs.pinned",
        "user.davfs.remote_size",
        "user.davfs.local_size",
        "user.davfs.etag",
    ];

    pub fn listxattr(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        for name in Self::XATTR_NAMES {
            buf.extend_from_slice(name.as_bytes());
            buf.push(0);
        }
        buf
    }

    pub async fn getxattr(self: &Arc<Self>, ino: u64, name: &str) -> Result<Vec<u8>, RemoteError> {
        let node = self.node_of(ino).await?;

        let value = match name {
            // The PoC decided dir-vs-file with `path.ends_with('/')`, which was
            // never true, so directories could never report `cached`.
            "user.davfs.state" => {
                if node.is_dir() {
                    let fresh = self
                        .meta
                        .children(&node.id)
                        .await
                        .map(|l| l.is_fresh(self.config.ttl))
                        .unwrap_or(false);
                    if fresh {
                        "cached".to_string()
                    } else {
                        "cloud".to_string()
                    }
                } else if node.is_cached() {
                    "cached".to_string()
                } else {
                    "cloud".to_string()
                }
            }
            "user.davfs.status" => node.sync_state.as_status(node.is_cached()).to_string(),
            "user.davfs.pinned" => u8::from(node.pinned).to_string(),
            "user.davfs.remote_size" => match node.size {
                SizeHint::Exact(n) => n.to_string(),
                SizeHint::Unknown => "unknown".to_string(),
            },
            "user.davfs.local_size" => node.content_len.unwrap_or(0).to_string(),
            "user.davfs.etag" => node
                .validator
                .as_ref()
                .map(|v| v.value())
                .unwrap_or_default(),
            _ => return Err(RemoteError::NotFound), // -> ENODATA at the shim
        };
        Ok(value.into_bytes())
    }

    /// Action attributes. These are the point of "sync control" and need no
    /// write path of their own.
    pub async fn setxattr(
        self: &Arc<Self>,
        ino: u64,
        name: &str,
        _value: &[u8],
    ) -> Result<(), RemoteError> {
        let node = self.node_of(ino).await?;

        match name {
            "user.davfs.do_pin" => {
                self.meta.set_pinned(&node.id, true).await.map_err(io_err)?;
                // Pinning implies keeping the content, so fetch it now.
                if !node.is_dir() && !node.is_cached() {
                    let handle = self.download(&node)?;
                    tokio::spawn(async move {
                        let _ = handle.await;
                    });
                }
                Ok(())
            }
            "user.davfs.do_unpin" => self.meta.set_pinned(&node.id, false).await.map_err(io_err),
            "user.davfs.do_download" => {
                if node.is_dir() {
                    return Err(RemoteError::Unsupported("download of a directory"));
                }
                let handle = self.download(&node)?;
                handle.await.map_err(unwrap_arc)?;
                Ok(())
            }
            "user.davfs.do_free" => {
                if node.pinned {
                    return Err(RemoteError::Unsupported("free of a pinned node"));
                }
                // Database first, then the file: never leave a row pointing at
                // a blob that is gone.
                if let Some(key) = self.meta.clear_content(&node.id).await.map_err(io_err)? {
                    let _ = self.content.remove(&key);
                }
                Ok(())
            }
            "user.davfs.do_refresh" => {
                let target = if node.is_dir() {
                    node.id.clone()
                } else {
                    node.parent.clone().unwrap_or_else(|| self.remote.root())
                };
                self.meta
                    .invalidate_listing(&target)
                    .await
                    .map_err(io_err)?;
                self.refresh_now(&target).await
            }
            _ => Err(RemoteError::Unsupported("unknown attribute")),
        }
    }

    // -- filesystem-wide --------------------------------------------------

    /// `statfs`. The PoC left this at `ENOSYS`, which makes `df` and several
    /// GTK file choosers misbehave.
    pub async fn statfs(&self) -> Statfs {
        let used = self.meta.total_content_bytes().await.unwrap_or(0);
        let blocks = 1u64 << 30; // a plausible 4 TiB at 4 KiB blocks
        Statfs {
            blocks,
            bfree: blocks.saturating_sub(used / 4096),
            files: 1 << 20,
            ffree: 1 << 19,
            bsize: 4096,
            namelen: 255,
        }
    }

    pub fn remote(&self) -> &Arc<dyn RemoteStorage> {
        &self.remote
    }

    pub fn meta(&self) -> &MetaStore {
        &self.meta
    }

    /// Populate the cache before the mount is exposed.
    ///
    /// A bounded breadth-first crawl of `Depth: 1` listings, concurrent per
    /// level. Never `Depth: infinity`, which Nextcloud and SabreDAV reject.
    pub async fn initial_sync(self: &Arc<Self>, max_depth: usize) -> Result<usize, RemoteError> {
        use futures::stream::{self, StreamExt};

        let mut frontier = vec![self.remote.root()];
        let mut listed = 0usize;

        for depth in 0..max_depth {
            if frontier.is_empty() {
                break;
            }
            tracing::info!("initial sync: level {} ({} dirs)", depth, frontier.len());

            let results: Vec<(NodeId, bool)> = stream::iter(frontier.clone())
                .map(|dir| {
                    let this = self.clone();
                    async move {
                        let ok = this.refresh_now(&dir).await.is_ok();
                        (dir, ok)
                    }
                })
                .buffer_unordered(self.config.max_concurrent)
                .collect()
                .await;

            let mut next = Vec::new();
            for (dir, ok) in results {
                if !ok {
                    continue;
                }
                listed += 1;
                if let Some(listing) = self.meta.children(&dir).await {
                    next.extend(
                        listing
                            .children
                            .into_iter()
                            .filter(|c| c.kind == NodeKind::Dir)
                            .map(|c| c.id),
                    );
                }
            }
            frontier = next;
        }
        Ok(listed)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Statfs {
    pub blocks: u64,
    pub bfree: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
}

fn io_err(e: anyhow::Error) -> RemoteError {
    RemoteError::Protocol(e.to_string())
}

/// Unwrap the `Arc<RemoteError>` a shared future hands back.
///
/// `RemoteError` is not `Clone`, and a shared future can hand the same error to
/// several waiters. Reconstructing by variant rather than falling back to
/// `Protocol` is what keeps the errno intact -- otherwise every waiter but one
/// would see EIO, and "the server is unreachable" would stop being
/// distinguishable from "the server is broken".
fn unwrap_arc(e: Arc<RemoteError>) -> RemoteError {
    match Arc::try_unwrap(e) {
        Ok(e) => e,
        Err(shared) => clone_error(&shared),
    }
}

fn clone_error(e: &RemoteError) -> RemoteError {
    match e {
        RemoteError::Unauthorized => RemoteError::Unauthorized,
        RemoteError::Forbidden => RemoteError::Forbidden,
        RemoteError::AuthExpired => RemoteError::AuthExpired,
        RemoteError::NotFound => RemoteError::NotFound,
        RemoteError::Exists => RemoteError::Exists,
        RemoteError::Changed => RemoteError::Changed,
        RemoteError::Locked => RemoteError::Locked,
        RemoteError::RateLimited(d) => RemoteError::RateLimited(*d),
        RemoteError::QuotaExceeded => RemoteError::QuotaExceeded,
        RemoteError::Offline(m) => RemoteError::Offline(m.clone()),
        RemoteError::Server { status, body } => RemoteError::Server {
            status: *status,
            body: body.clone(),
        },
        RemoteError::Protocol(m) => RemoteError::Protocol(m.clone()),
        RemoteError::Unsupported(m) => RemoteError::Unsupported(m),
        RemoteError::Io(io) => RemoteError::Io(std::io::Error::new(io.kind(), io.to_string())),
    }
}
