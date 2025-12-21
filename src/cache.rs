use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::webdav::DavEntry;

#[derive(Clone)]
pub struct DirectoryCache {
    entries: Arc<Mutex<HashMap<String, CachedDirectory>>>,
    ttl: Duration,
}

struct CachedDirectory {
    entries: Vec<DavEntry>,
    cached_at: Instant,
}

impl DirectoryCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            ttl,
        }
    }

    pub fn get(&self, path: &str) -> Option<Vec<DavEntry>> {
        let entries = self.entries.lock().unwrap();
        
        if let Some(cached) = entries.get(path) {
            if cached.cached_at.elapsed() < self.ttl {
                tracing::debug!("Cache hit for path: {}", path);
                return Some(cached.entries.clone());
            } else {
                tracing::debug!("Cache expired for path: {}", path);
            }
        } else {
            tracing::debug!("Cache miss for path: {}", path);
        }
        
        None
    }

    pub fn insert(&self, path: String, entries: Vec<DavEntry>) {
        let mut cache = self.entries.lock().unwrap();
        cache.insert(path.clone(), CachedDirectory {
            entries,
            cached_at: Instant::now(),
        });
        tracing::debug!("Cached {} entries for path: {}", cache.get(&path).map(|c| c.entries.len()).unwrap_or(0), path);
    }

    pub fn invalidate(&self, path: &str) {
        let mut cache = self.entries.lock().unwrap();
        cache.remove(path);
        tracing::debug!("Invalidated cache for path: {}", path);
    }

    pub fn clear(&self) {
        let mut cache = self.entries.lock().unwrap();
        cache.clear();
        tracing::info!("Cleared all cache entries");
    }

    pub fn stats(&self) -> CacheStats {
        let cache = self.entries.lock().unwrap();
        let total_entries = cache.len();
        let expired = cache.values()
            .filter(|c| c.cached_at.elapsed() >= self.ttl)
            .count();
        
        CacheStats {
            total_directories: total_entries,
            expired_directories: expired,
            active_directories: total_entries - expired,
        }
    }
}

pub struct CacheStats {
    pub total_directories: usize,
    pub expired_directories: usize,
    pub active_directories: usize,
}
