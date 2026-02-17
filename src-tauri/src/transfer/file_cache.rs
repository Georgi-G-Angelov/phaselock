use std::collections::HashMap;
use uuid::Uuid;

/// In-memory cache of received MP3 files, keyed by file_id.
///
/// Files remain cached for the duration of the app session, so that
/// reconnecting peers don't need to re-download files they already have.
#[derive(Debug, Default)]
pub struct FileCache {
    files: HashMap<Uuid, Vec<u8>>,
}

impl FileCache {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
        }
    }

    /// Check whether a file is already cached.
    pub fn has(&self, file_id: &Uuid) -> bool {
        self.files.contains_key(file_id)
    }

    /// Retrieve the raw MP3 bytes for a cached file.
    pub fn get(&self, file_id: &Uuid) -> Option<&Vec<u8>> {
        self.files.get(file_id)
    }

    /// Insert (or replace) a file in the cache.
    pub fn insert(&mut self, file_id: Uuid, data: Vec<u8>) {
        self.files.insert(file_id, data);
    }

    /// Return the set of file IDs currently in the cache.
    pub fn cached_ids(&self) -> Vec<Uuid> {
        self.files.keys().copied().collect()
    }

    /// Remove a specific file from the cache.
    pub fn remove(&mut self, file_id: &Uuid) -> Option<Vec<u8>> {
        self.files.remove(file_id)
    }

    /// Clear all cached files.
    pub fn clear(&mut self) {
        self.files.clear();
    }

    /// Number of cached files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_get() {
        let mut cache = FileCache::new();
        let id = Uuid::new_v4();
        let data = vec![1, 2, 3, 4, 5];

        assert!(!cache.has(&id));
        assert!(cache.get(&id).is_none());

        cache.insert(id, data.clone());

        assert!(cache.has(&id));
        assert_eq!(cache.get(&id), Some(&data));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_cached_ids() {
        let mut cache = FileCache::new();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        cache.insert(id1, vec![1]);
        cache.insert(id2, vec![2]);

        let ids = cache.cached_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
    }

    #[test]
    fn test_remove() {
        let mut cache = FileCache::new();
        let id = Uuid::new_v4();
        cache.insert(id, vec![10, 20]);

        assert!(cache.has(&id));
        let removed = cache.remove(&id);
        assert_eq!(removed, Some(vec![10, 20]));
        assert!(!cache.has(&id));
    }

    #[test]
    fn test_clear() {
        let mut cache = FileCache::new();
        cache.insert(Uuid::new_v4(), vec![1]);
        cache.insert(Uuid::new_v4(), vec![2]);

        assert_eq!(cache.len(), 2);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn test_insert_replaces_existing() {
        let mut cache = FileCache::new();
        let id = Uuid::new_v4();
        cache.insert(id, vec![1, 2, 3]);
        cache.insert(id, vec![4, 5, 6]);

        assert_eq!(cache.get(&id), Some(&vec![4, 5, 6]));
        assert_eq!(cache.len(), 1);
    }
}
