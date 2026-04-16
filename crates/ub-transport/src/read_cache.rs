use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Key for the read response cache: (source node, opaque ID).
pub type CacheKey = (u16, u64);

/// An entry in the read response cache.
struct CacheEntry {
    response: Vec<u8>,
    inserted_at: Instant,
    /// Order index for LRU eviction.
    order: u64,
}

/// LRU cache for read response deduplication.
///
/// When a Read request arrives and has already been processed, the cached
/// response is returned instead of re-executing the read. This provides
/// idempotency for retransmitted Read requests.
///
/// Uses a simple LRU implementation: HashMap for O(1) lookup + order counter
/// for O(n) eviction. Sufficient for the toy implementation's capacity.
pub struct ReadResponseCache {
    entries: HashMap<CacheKey, CacheEntry>,
    capacity: usize,
    ttl: Duration,
    next_order: u64,
}

impl ReadResponseCache {
    /// Create a new cache with the given capacity and TTL.
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        ReadResponseCache {
            entries: HashMap::new(),
            capacity,
            ttl,
            next_order: 0,
        }
    }

    /// Create a cache with default settings (capacity=1024, TTL=5s).
    pub fn default_cache() -> Self {
        Self::new(1024, Duration::from_secs(5))
    }

    /// Look up a cached response. Returns a cloned copy of the cached frame
    /// bytes if found and not expired. Updates LRU order on hit.
    pub fn get(&mut self, key: CacheKey) -> Option<Vec<u8>> {
        // Check existence and expiry first
        let is_expired = self.entries.get(&key).map_or(true, |e| e.inserted_at.elapsed() > self.ttl);
        if is_expired {
            self.entries.remove(&key);
            return None;
        }

        // Update LRU order and return
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.order = self.next_order;
            self.next_order += 1;
            return Some(entry.response.clone());
        }
        None
    }

    /// Insert a response into the cache. If at capacity, evicts the least
    /// recently used entry.
    pub fn insert(&mut self, key: CacheKey, response: Vec<u8>) {
        // Remove expired entries first
        self.remove_expired();

        // Evict LRU if at capacity
        if self.entries.len() >= self.capacity {
            if let Some(lru_key) = self.find_lru_key() {
                self.entries.remove(&lru_key);
            }
        }

        let order = self.next_order;
        self.next_order += 1;

        self.entries.insert(key, CacheEntry {
            response,
            inserted_at: Instant::now(),
            order,
        });
    }

    /// Remove all expired entries.
    pub fn remove_expired(&mut self) {
        let now = Instant::now();
        self.entries.retain(|_, entry| now.duration_since(entry.inserted_at) <= self.ttl);
    }

    /// Clear the entire cache.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Get the number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn find_lru_key(&self) -> Option<CacheKey> {
        self.entries.iter()
            .min_by_key(|(_, e)| e.order)
            .map(|(k, _)| *k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_get() {
        let mut cache = ReadResponseCache::new(10, Duration::from_secs(5));
        let key = (1u16, 42u64);
        let response = vec![1, 2, 3, 4];

        cache.insert(key, response.clone());
        let result = cache.get(key);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), response);

        // Non-existent key
        assert!(cache.get((2, 99)).is_none());
    }

    #[test]
    fn test_capacity_eviction() {
        let mut cache = ReadResponseCache::new(3, Duration::from_secs(60));

        // Insert 3 entries
        cache.insert((1, 1), vec![1]);
        cache.insert((1, 2), vec![2]);
        cache.insert((1, 3), vec![3]);
        assert_eq!(cache.len(), 3);

        // Access entry (1,1) to make it recently used
        let _ = cache.get((1, 1));

        // Insert a 4th — should evict (1,2) as LRU
        cache.insert((1, 4), vec![4]);
        assert_eq!(cache.len(), 3);
        assert!(cache.get((1, 2)).is_none()); // evicted
        assert!(cache.get((1, 1)).is_some()); // still present (accessed recently)
        assert!(cache.get((1, 3)).is_some());
        assert!(cache.get((1, 4)).is_some());
    }

    #[test]
    fn test_expired_entries() {
        let mut cache = ReadResponseCache::new(10, Duration::from_millis(50));
        cache.insert((1, 1), vec![1]);

        // Should be available immediately
        assert!(cache.get((1, 1)).is_some());

        // Wait for expiry
        std::thread::sleep(Duration::from_millis(60));

        // Should be expired now
        assert!(cache.get((1, 1)).is_none());
    }

    #[test]
    fn test_remove_expired() {
        let mut cache = ReadResponseCache::new(10, Duration::from_millis(50));
        cache.insert((1, 1), vec![1]);
        cache.insert((1, 2), vec![2]);

        std::thread::sleep(Duration::from_millis(60));

        cache.remove_expired();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_clear() {
        let mut cache = ReadResponseCache::new(10, Duration::from_secs(5));
        cache.insert((1, 1), vec![1]);
        cache.insert((1, 2), vec![2]);
        assert_eq!(cache.len(), 2);
        cache.clear();
        assert!(cache.is_empty());
    }
}