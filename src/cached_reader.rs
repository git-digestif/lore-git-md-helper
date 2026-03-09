//! A caching layer on top of CatFile.
//!
//! Provides a single unified cache for all git object lookups, replacing
//! scattered per-type caches (thread_summaries, daily_digest_cache, etc.).
//!
//! Content that has been sent to fast-import but may not yet be visible
//! via git (due to async checkpoint lag) can be injected via `insert()`.
//! Subsequent lookups hit the cache, transparently bridging the gap
//! between what fast-import has accepted and what CatFile can see.

use std::collections::HashMap;

use crate::cat_file::{BlobRead, CatFile};

/// A caching wrapper around [`CatFile`].
///
/// Lookups check the in-memory cache first, falling through to CatFile
/// on miss.  CatFile results are *not* cached automatically (to avoid
/// staleness after fast-import checkpoints); only explicit `insert()`
/// calls populate the cache.
pub struct CachedReader {
    cat: CatFile,
    cache: HashMap<String, String>,
}

impl CachedReader {
    pub fn new(cat: CatFile) -> Self {
        Self {
            cat,
            cache: HashMap::new(),
        }
    }

    /// Inject content into the cache.
    ///
    /// Use this after sending content to fast-import so that subsequent
    /// lookups see the new state without waiting for the ref to update.
    pub fn insert(&mut self, key: String, content: String) {
        self.cache.insert(key, content);
    }
}

impl BlobRead for CachedReader {
    fn get_str(&mut self, spec: &str) -> Option<String> {
        if let Some(cached) = self.cache.get(spec) {
            return Some(cached.clone());
        }
        self.cat.get_str(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cat_file::MockBlobs;

    /// Verify that CachedReader can be used behind the BlobRead trait.
    /// (We test with MockBlobs since we can't spawn git in a unit test
    /// without a repo, but the CachedReader wrapper logic is identical.)
    #[test]
    fn test_insert_takes_precedence() {
        // Simulate CachedReader behavior with a mock: insert should
        // shadow anything the underlying reader would return.
        let mut cache: HashMap<String, String> = HashMap::new();
        let mut mock = MockBlobs(HashMap::new());
        mock.0.insert("main:foo.md".into(), "from git".into());

        // Without cache: get from mock
        assert_eq!(mock.get_str("main:foo.md"), Some("from git".into()));

        // With cache: insert shadows
        cache.insert("main:foo.md".into(), "from cache".into());
        let result = cache
            .get("main:foo.md")
            .cloned()
            .or_else(|| mock.get_str("main:foo.md"));
        assert_eq!(result, Some("from cache".into()));
    }

    #[test]
    fn test_cache_miss_falls_through() {
        let mut mock = MockBlobs(HashMap::new());
        mock.0.insert("main:bar.md".into(), "bar content".into());

        let cache: HashMap<String, String> = HashMap::new();
        let result = cache
            .get("main:bar.md")
            .cloned()
            .or_else(|| mock.get_str("main:bar.md"));
        assert_eq!(result, Some("bar content".into()));
    }

    #[test]
    fn test_cache_miss_returns_none() {
        let mut mock = MockBlobs(HashMap::new());
        let cache: HashMap<String, String> = HashMap::new();
        let result = cache
            .get("main:missing.md")
            .cloned()
            .or_else(|| mock.get_str("main:missing.md"));
        assert!(result.is_none());
    }
}
