//! An in-memory [`RemoteStorage`] for tests.
//!
//! Lets the cache, the VFS and the FUSE layer be tested without HTTP, a server,
//! or a mount -- and lets failure modes that are awkward to provoke against a
//! real server (rate limiting, a mid-read change, going offline) be triggered
//! exactly.
//!
//! It is also the second implementation of the trait, which is what stops the
//! seam from quietly becoming WebDAV-shaped.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

use bytes::Bytes;

use super::{
    ByteRange, Capabilities, Change, ChangeBatch, ChangeFeed, ContentSource, ContentStream,
    Listing, NodeId, NodeKind, RemoteEntry, RemoteStorage, SizeHint, Validator,
};
use crate::error::RemoteError;

#[derive(Default)]
struct State {
    /// id -> entry
    nodes: HashMap<NodeId, RemoteEntry>,
    /// id -> content
    content: HashMap<NodeId, Bytes>,
    /// directory id -> its validator
    dir_validators: HashMap<NodeId, Validator>,
    /// Appended to by mutations; drained by `poll_changes`.
    changes: Vec<Change>,
}

/// Configurable in-memory backend.
pub struct MockRemote {
    state: Mutex<State>,
    caps: Capabilities,
    feed: ChangeFeed,
    /// When set, every operation fails with this error.
    fail_with: Mutex<Option<RemoteError>>,
    offline: AtomicBool,
    /// Counts of each operation, for asserting on request volume.
    pub list_calls: AtomicU32,
    pub read_calls: AtomicU32,
}

impl Default for MockRemote {
    fn default() -> Self {
        Self::new()
    }
}

impl MockRemote {
    /// A path-addressed backend that behaves like WebDAV.
    pub fn new() -> Self {
        let mut state = State::default();
        state
            .nodes
            .insert(NodeId("/".into()), RemoteEntry::dir(NodeId("/".into()), ""));
        Self {
            state: Mutex::new(state),
            caps: Capabilities {
                path_addressed: true,
                unique_names: true,
                dir_validators: true,
                ranged_reads: true,
                preferred_chunk: None,
                max_upload_size: None,
                enforces_preconditions: true,
            },
            feed: ChangeFeed::PerDirectory,
            fail_with: Mutex::new(None),
            offline: AtomicBool::new(false),
            list_calls: AtomicU32::new(0),
            read_calls: AtomicU32::new(0),
        }
    }

    /// An id-addressed backend with duplicate names and a cursor feed -- the
    /// Google Drive shape. Used to prove the seam is not WebDAV-specific.
    pub fn id_addressed() -> Self {
        let mut m = Self::new();
        m.caps = Capabilities {
            path_addressed: false,
            unique_names: false,
            dir_validators: false,
            ranged_reads: true,
            preferred_chunk: Some(8 * 1024 * 1024),
            max_upload_size: None,
            enforces_preconditions: true,
        };
        m.feed = ChangeFeed::Cursor;
        m
    }

    /// Add a file. `parent` must already exist.
    pub fn add_file(&self, parent: &str, name: &str, content: &[u8]) -> NodeId {
        let id = self.mint_id(parent, name);
        let mut st = self.state.lock().unwrap();
        let entry = RemoteEntry {
            parent: Some(NodeId(parent.to_string())),
            validator: Some(Validator::Etag(format!("\"{}-{}\"", name, content.len()))),
            ..RemoteEntry::file(id.clone(), name, content.len() as u64)
        };
        st.content
            .insert(id.clone(), Bytes::copy_from_slice(content));
        st.changes.push(Change::Upserted(Box::new(entry.clone())));
        st.nodes.insert(id.clone(), entry);
        id
    }

    pub fn add_dir(&self, parent: &str, name: &str) -> NodeId {
        let id = self.mint_id(parent, name);
        let mut st = self.state.lock().unwrap();
        let entry = RemoteEntry {
            parent: Some(NodeId(parent.to_string())),
            ..RemoteEntry::dir(id.clone(), name)
        };
        st.changes.push(Change::Upserted(Box::new(entry.clone())));
        st.nodes.insert(id.clone(), entry);
        id
    }

    /// A Google-native document: readable, but with no size until exported.
    pub fn add_export_only(&self, parent: &str, name: &str, exported: &[u8]) -> NodeId {
        let id = self.mint_id(parent, name);
        let mut st = self.state.lock().unwrap();
        let entry = RemoteEntry {
            parent: Some(NodeId(parent.to_string())),
            kind: NodeKind::ExportOnly,
            size: SizeHint::Unknown,
            ..RemoteEntry::file(id.clone(), name, 0)
        };
        st.content
            .insert(id.clone(), Bytes::copy_from_slice(exported));
        st.nodes.insert(id.clone(), entry);
        id
    }

    fn mint_id(&self, parent: &str, name: &str) -> NodeId {
        if self.caps.path_addressed {
            if parent == "/" {
                NodeId(format!("/{name}"))
            } else {
                NodeId(format!("{}/{}", parent.trim_end_matches('/'), name))
            }
        } else {
            // Opaque and unrelated to the name, so a test that accidentally
            // assumes path addressing fails loudly.
            let n = self.state.lock().unwrap().nodes.len();
            NodeId(format!("id-{n:04}"))
        }
    }

    /// Change a file's content, which changes its validator.
    pub fn set_content(&self, id: &NodeId, content: &[u8]) {
        let mut st = self.state.lock().unwrap();
        st.content
            .insert(id.clone(), Bytes::copy_from_slice(content));
        if let Some(e) = st.nodes.get_mut(id) {
            e.size = SizeHint::Exact(content.len() as u64);
            e.validator = Some(Validator::Etag(format!("\"v{}\"", content.len())));
        }
        let entry = st.nodes.get(id).cloned();
        if let Some(entry) = entry {
            st.changes.push(Change::Upserted(Box::new(entry)));
        }
    }

    pub fn remove_node(&self, id: &NodeId) {
        let mut st = self.state.lock().unwrap();
        st.nodes.remove(id);
        st.content.remove(id);
        st.changes.push(Change::Removed(id.clone()));
    }

    /// Set a directory's listing validator, so `Listing::Unchanged` can fire.
    pub fn set_dir_validator(&self, dir: &NodeId, v: Validator) {
        self.state
            .lock()
            .unwrap()
            .dir_validators
            .insert(dir.clone(), v);
    }

    /// Make every subsequent operation fail with `err`.
    pub fn fail_with(&self, err: RemoteError) {
        *self.fail_with.lock().unwrap() = Some(err);
    }

    pub fn clear_failure(&self) {
        *self.fail_with.lock().unwrap() = None;
        self.offline.store(false, Ordering::SeqCst);
    }

    /// Simulate loss of connectivity.
    pub fn go_offline(&self) {
        self.offline.store(true, Ordering::SeqCst);
    }

    fn check_health(&self) -> Result<(), RemoteError> {
        if self.offline.load(Ordering::SeqCst) {
            return Err(RemoteError::Offline("mock is offline".into()));
        }
        if let Some(e) = self.fail_with.lock().unwrap().as_ref() {
            // RemoteError is not Clone; reconstruct the salient part.
            return Err(match e {
                RemoteError::Unauthorized => RemoteError::Unauthorized,
                RemoteError::NotFound => RemoteError::NotFound,
                RemoteError::RateLimited(d) => RemoteError::RateLimited(*d),
                RemoteError::Changed => RemoteError::Changed,
                other => RemoteError::Protocol(other.to_string()),
            });
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl RemoteStorage for MockRemote {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn change_feed(&self) -> ChangeFeed {
        self.feed
    }

    fn root(&self) -> NodeId {
        NodeId("/".into())
    }

    fn child_id(&self, parent: &NodeId, name: &str) -> Option<NodeId> {
        self.caps
            .path_addressed
            .then(|| self.mint_id(parent.as_str(), name))
    }

    async fn probe(&self) -> Result<(), RemoteError> {
        self.check_health()
    }

    async fn list(
        &self,
        dir: &NodeId,
        if_unchanged: Option<&Validator>,
    ) -> Result<Listing, RemoteError> {
        self.check_health()?;
        self.list_calls.fetch_add(1, Ordering::SeqCst);

        let st = self.state.lock().unwrap();
        if !st.nodes.contains_key(dir) {
            return Err(RemoteError::NotFound);
        }

        let current = st.dir_validators.get(dir);
        if let (Some(want), Some(have)) = (if_unchanged, current) {
            if want == have {
                return Ok(Listing::Unchanged);
            }
        }

        let children: Vec<RemoteEntry> = st
            .nodes
            .values()
            .filter(|e| e.parent.as_ref() == Some(dir))
            .cloned()
            .collect();

        Ok(Listing::Fresh {
            validator: current.cloned(),
            this: st.nodes.get(dir).cloned().map(Box::new),
            children,
            complete: true,
        })
    }

    async fn stat(&self, node: &NodeId) -> Result<RemoteEntry, RemoteError> {
        self.check_health()?;
        self.state
            .lock()
            .unwrap()
            .nodes
            .get(node)
            .cloned()
            .ok_or(RemoteError::NotFound)
    }

    async fn open_read(
        &self,
        node: &NodeId,
        range: Option<ByteRange>,
        if_unchanged: Option<&Validator>,
    ) -> Result<ContentStream, RemoteError> {
        self.check_health()?;
        self.read_calls.fetch_add(1, Ordering::SeqCst);

        let st = self.state.lock().unwrap();
        let entry = st.nodes.get(node).ok_or(RemoteError::NotFound)?;

        if let (Some(want), Some(have)) = (if_unchanged, entry.validator.as_ref()) {
            if want != have {
                return Err(RemoteError::Changed);
            }
        }

        let full = st.content.get(node).cloned().ok_or(RemoteError::NotFound)?;
        let total = full.len() as u64;

        let slice = match range {
            None => full,
            Some(r) => {
                let start = (r.offset as usize).min(full.len());
                let end = match r.len {
                    Some(l) => (start + l as usize).min(full.len()),
                    None => full.len(),
                };
                full.slice(start..end)
            }
        };

        Ok(ContentStream {
            validator: entry.validator.clone(),
            total_size: Some(total),
            body: Box::pin(futures::stream::once(async move { Ok(slice) })),
        })
    }

    async fn start_cursor(&self) -> Result<String, RemoteError> {
        if self.feed != ChangeFeed::Cursor {
            return Err(RemoteError::Unsupported("start_cursor"));
        }
        Ok(self.state.lock().unwrap().changes.len().to_string())
    }

    async fn poll_changes(&self, cursor: &str) -> Result<ChangeBatch, RemoteError> {
        if self.feed != ChangeFeed::Cursor {
            return Err(RemoteError::Unsupported("poll_changes"));
        }
        self.check_health()?;

        let st = self.state.lock().unwrap();
        let from: usize = cursor.parse().unwrap_or(0);
        let changes = st.changes.get(from..).unwrap_or_default().to_vec();

        Ok(ChangeBatch {
            cursor: st.changes.len().to_string(),
            changes,
            more: false,
        })
    }

    async fn create_dir(&self, parent: &NodeId, name: &str) -> Result<RemoteEntry, RemoteError> {
        self.check_health()?;
        let id = self.add_dir(parent.as_str(), name);
        self.stat(&id).await
    }

    async fn create_file(
        &self,
        parent: &NodeId,
        name: &str,
        _mime: Option<&str>,
    ) -> Result<RemoteEntry, RemoteError> {
        self.check_health()?;
        let id = self.add_file(parent.as_str(), name, b"");
        self.stat(&id).await
    }

    async fn write_content(
        &self,
        node: &NodeId,
        body: ContentSource,
        if_unchanged: Option<&Validator>,
    ) -> Result<RemoteEntry, RemoteError> {
        self.check_health()?;
        {
            let st = self.state.lock().unwrap();
            let entry = st.nodes.get(node).ok_or(RemoteError::NotFound)?;
            if let (Some(want), Some(have)) = (if_unchanged, entry.validator.as_ref()) {
                if want != have {
                    return Err(RemoteError::Changed);
                }
            }
        }
        let bytes = match body {
            ContentSource::Bytes(b) => b.to_vec(),
            ContentSource::File { path, .. } => std::fs::read(path)?,
        };
        self.set_content(node, &bytes);
        self.stat(node).await
    }

    async fn remove(
        &self,
        node: &NodeId,
        if_unchanged: Option<&Validator>,
    ) -> Result<(), RemoteError> {
        self.check_health()?;
        {
            let st = self.state.lock().unwrap();
            let entry = st.nodes.get(node).ok_or(RemoteError::NotFound)?;
            if let (Some(want), Some(have)) = (if_unchanged, entry.validator.as_ref()) {
                if want != have {
                    return Err(RemoteError::Changed);
                }
            }
        }
        self.remove_node(node);
        Ok(())
    }
}
