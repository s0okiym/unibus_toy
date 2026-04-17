use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::RwLock;
use ub_core::types::{RegionId, RegionState};

use crate::region::RegionTable;
use crate::sub_alloc::SubAllocator;

/// A cached region entry in the local cache pool.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub region_id: RegionId,
    pub pool_offset: u64,
    pub len: u64,
    pub epoch: u64,
    pub last_access: Instant,
}

/// Cache pool — manages local cached copies of remote regions.
///
/// Uses a SubAllocator for memory management within a pool MR.
/// Eviction is simple LRU based on last_access time.
/// When an entry is evicted, the corresponding region's state in the
/// RegionTable is set to Invalid.
pub struct CachePool {
    entries: RwLock<HashMap<RegionId, CacheEntry>>,
    allocator: RwLock<SubAllocator>,
    max_bytes: u64,
    used_bytes: AtomicU64,
    /// Optional region table reference — when set, evicted entries
    /// have their region state transitioned to Invalid.
    region_table: Option<Arc<RegionTable>>,
}

impl CachePool {
    pub fn new(allocator: SubAllocator, max_bytes: u64) -> Self {
        CachePool {
            entries: RwLock::new(HashMap::new()),
            allocator: RwLock::new(allocator),
            max_bytes,
            used_bytes: AtomicU64::new(0),
            region_table: None,
        }
    }

    /// Create a CachePool with a RegionTable reference for eviction notification.
    pub fn with_region_table(allocator: SubAllocator, max_bytes: u64, region_table: Arc<RegionTable>) -> Self {
        CachePool {
            entries: RwLock::new(HashMap::new()),
            allocator: RwLock::new(allocator),
            max_bytes,
            used_bytes: AtomicU64::new(0),
            region_table: Some(region_table),
        }
    }

    /// Look up a cached region. Returns the cache entry if found, updating last_access.
    pub fn lookup(&self, region_id: RegionId) -> Option<CacheEntry> {
        let mut entries = self.entries.write();
        if let Some(entry) = entries.get_mut(&region_id) {
            entry.last_access = Instant::now();
            return Some(entry.clone());
        }
        None
    }

    /// Insert a new cache entry. Allocates space from the sub-allocator.
    /// If the pool is full, evicts the LRU entry first.
    pub fn insert(&self, region_id: RegionId, len: u64, epoch: u64) -> Result<CacheEntry, String> {
        // Evict if needed
        while self.used_bytes.load(Ordering::Acquire) + len > self.max_bytes {
            if !self.evict_lru() {
                return Err("cache pool full, cannot evict enough space".to_string());
            }
        }

        // Allocate from sub-allocator
        let pool_offset = self.allocator.write().alloc(len)
            .ok_or("sub-allocator exhausted")?;

        let entry = CacheEntry {
            region_id,
            pool_offset,
            len,
            epoch,
            last_access: Instant::now(),
        };

        self.used_bytes.fetch_add(len, Ordering::AcqRel);
        self.entries.write().insert(region_id, entry.clone());

        Ok(entry)
    }

    /// Remove a cache entry. Returns the removed entry if found.
    pub fn remove(&self, region_id: RegionId) -> Option<CacheEntry> {
        let mut entries = self.entries.write();
        let entry = entries.remove(&region_id)?;
        self.used_bytes.fetch_sub(entry.len, Ordering::AcqRel);
        // Note: sub-allocator free is a no-op (bump allocator)
        self.allocator.read().free(entry.pool_offset, entry.len);
        Some(entry)
    }

    /// Evict the least recently used entry. Returns true if an entry was evicted.
    /// If a RegionTable is set, the evicted region's state is set to Invalid.
    pub fn evict_lru(&self) -> bool {
        let mut entries = self.entries.write();
        if entries.is_empty() {
            return false;
        }

        // Find the LRU entry
        let lru_key = entries.iter()
            .min_by_key(|(_, e)| e.last_access)
            .map(|(k, _)| *k);

        if let Some(key) = lru_key {
            let entry = entries.remove(&key).unwrap();
            self.used_bytes.fetch_sub(entry.len, Ordering::AcqRel);
            self.allocator.read().free(entry.pool_offset, entry.len);

            // Notify region table that this region's cache was evicted
            if let Some(ref region_table) = self.region_table {
                region_table.set_state(entry.region_id, RegionState::Invalid);
            }

            return true;
        }
        false
    }

    /// Update the epoch of a cached region.
    pub fn update_epoch(&self, region_id: RegionId, epoch: u64) -> bool {
        let mut entries = self.entries.write();
        if let Some(entry) = entries.get_mut(&region_id) {
            entry.epoch = epoch;
            entry.last_access = Instant::now();
            true
        } else {
            false
        }
    }

    /// Check if the pool contains a cached entry for the given region.
    pub fn contains(&self, region_id: RegionId) -> bool {
        self.entries.read().contains_key(&region_id)
    }

    /// Current number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    /// Current used bytes.
    pub fn used_bytes(&self) -> u64 {
        self.used_bytes.load(Ordering::Acquire)
    }

    /// Max bytes.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::region::RegionTable;
    use ub_core::types::RegionInfo;

    fn make_pool(max_bytes: u64) -> CachePool {
        CachePool::new(SubAllocator::new(0, max_bytes * 2), max_bytes)
    }

    fn make_pool_with_region_table(max_bytes: u64) -> (CachePool, Arc<RegionTable>) {
        let rt = Arc::new(RegionTable::new());
        let pool = CachePool::with_region_table(
            SubAllocator::new(0, max_bytes * 2),
            max_bytes,
            Arc::clone(&rt),
        );
        (pool, rt)
    }

    fn make_region_info(region_id: RegionId, state: RegionState) -> RegionInfo {
        RegionInfo {
            region_id,
            home_node_id: 1,
            device_id: 0,
            mr_handle: 0,
            base_offset: 0,
            len: 1024,
            epoch: 0,
            state,
            local_mr_handle: None,
        }
    }

    #[test]
    fn test_cache_pool_insert_and_lookup() {
        let pool = make_pool(4096);
        let entry = pool.insert(RegionId(1), 1024, 0).unwrap();
        assert_eq!(entry.region_id, RegionId(1));
        assert_eq!(entry.epoch, 0);

        let found = pool.lookup(RegionId(1)).unwrap();
        assert_eq!(found.region_id, RegionId(1));
        assert_eq!(found.pool_offset, entry.pool_offset);
    }

    #[test]
    fn test_cache_pool_miss() {
        let pool = make_pool(4096);
        assert!(pool.lookup(RegionId(99)).is_none());
    }

    #[test]
    fn test_cache_pool_eviction() {
        let pool = make_pool(2048); // Only fits 2 entries of 1024

        pool.insert(RegionId(1), 1024, 0).unwrap();
        pool.insert(RegionId(2), 1024, 0).unwrap();

        // Inserting a third should evict the LRU (region 1)
        pool.insert(RegionId(3), 1024, 0).unwrap();

        assert!(pool.lookup(RegionId(1)).is_none()); // evicted
        assert!(pool.lookup(RegionId(2)).is_some()); // still there
        assert!(pool.lookup(RegionId(3)).is_some()); // just inserted
    }

    #[test]
    fn test_cache_pool_remove() {
        let pool = make_pool(4096);
        pool.insert(RegionId(1), 1024, 0).unwrap();
        let removed = pool.remove(RegionId(1)).unwrap();
        assert_eq!(removed.region_id, RegionId(1));
        assert!(pool.lookup(RegionId(1)).is_none());
    }

    #[test]
    fn test_cache_pool_update_epoch() {
        let pool = make_pool(4096);
        pool.insert(RegionId(1), 1024, 0).unwrap();
        assert!(pool.update_epoch(RegionId(1), 5));
        let found = pool.lookup(RegionId(1)).unwrap();
        assert_eq!(found.epoch, 5);
        assert!(!pool.update_epoch(RegionId(99), 1));
    }

    #[test]
    fn test_cache_pool_access_updates_lru() {
        let pool = make_pool(2048);

        pool.insert(RegionId(1), 1024, 0).unwrap();
        pool.insert(RegionId(2), 1024, 0).unwrap();

        // Access region 1 (making it more recent)
        let _ = pool.lookup(RegionId(1));

        // Inserting a third should evict the LRU (region 2, since we just accessed region 1)
        pool.insert(RegionId(3), 1024, 0).unwrap();

        assert!(pool.lookup(RegionId(1)).is_some()); // recently accessed
        assert!(pool.lookup(RegionId(2)).is_none()); // evicted (LRU)
        assert!(pool.lookup(RegionId(3)).is_some()); // just inserted
    }

    // --- Step 7.5 new tests ---

    #[test]
    fn test_lru_eviction_order() {
        // Eviction order strictly follows least-recently-accessed time
        let pool = make_pool(3072); // fits 3 entries of 1024

        pool.insert(RegionId(1), 1024, 0).unwrap();
        pool.insert(RegionId(2), 1024, 0).unwrap();
        pool.insert(RegionId(3), 1024, 0).unwrap();

        // All 3 inserted. Inserting a 4th evicts region 1 (oldest)
        pool.insert(RegionId(4), 1024, 0).unwrap();
        assert!(pool.lookup(RegionId(1)).is_none());
        assert!(pool.lookup(RegionId(2)).is_some());
        assert!(pool.lookup(RegionId(3)).is_some());
        assert!(pool.lookup(RegionId(4)).is_some());

        // Inserting a 5th evicts region 2 (next oldest)
        pool.insert(RegionId(5), 1024, 0).unwrap();
        assert!(pool.lookup(RegionId(2)).is_none());
        assert!(pool.lookup(RegionId(3)).is_some());
        assert!(pool.lookup(RegionId(4)).is_some());
        assert!(pool.lookup(RegionId(5)).is_some());
    }

    #[test]
    fn test_lru_access_updates_order() {
        // Accessing a region makes it the most recently used, preventing eviction
        let pool = make_pool(2048);

        pool.insert(RegionId(1), 1024, 0).unwrap();
        pool.insert(RegionId(2), 1024, 0).unwrap();

        // Access region 1 to make it recent
        let _ = pool.lookup(RegionId(1));

        // Inserting a third should evict region 2 (LRU), not region 1
        pool.insert(RegionId(3), 1024, 0).unwrap();
        assert!(pool.lookup(RegionId(1)).is_some());
        assert!(pool.lookup(RegionId(2)).is_none());

        // Now access region 3, then region 1 again
        let _ = pool.lookup(RegionId(3));
        let _ = pool.lookup(RegionId(1));

        // Inserting a 4th should evict region 3 was accessed before region 1,
        // but after our lookups region 3 was accessed before region 1.
        // Wait: order is 3 accessed, then 1 accessed. So 3 is LRU.
        pool.insert(RegionId(4), 1024, 0).unwrap();
        assert!(pool.lookup(RegionId(1)).is_some()); // most recently accessed
        assert!(pool.lookup(RegionId(3)).is_none()); // evicted
        assert!(pool.lookup(RegionId(4)).is_some());
    }

    #[test]
    fn test_cache_pool_capacity_enforcement() {
        // Pool auto-evicts when capacity would be exceeded
        let pool = make_pool(2048);

        pool.insert(RegionId(1), 1024, 0).unwrap();
        pool.insert(RegionId(2), 1024, 0).unwrap();
        assert_eq!(pool.used_bytes(), 2048);
        assert_eq!(pool.max_bytes(), 2048);

        // Insert a large entry that requires evicting both
        let entry = pool.insert(RegionId(3), 2048, 0).unwrap();
        assert_eq!(entry.len, 2048);
        assert_eq!(pool.used_bytes(), 2048);

        // Both old entries should be evicted
        assert!(pool.lookup(RegionId(1)).is_none());
        assert!(pool.lookup(RegionId(2)).is_none());
        assert!(pool.lookup(RegionId(3)).is_some());
    }

    #[test]
    fn test_cache_pool_eviction_invalidates_region() {
        // When a cache entry is evicted, the corresponding region's state in the
        // RegionTable should transition to Invalid
        let (pool, rt) = make_pool_with_region_table(2048);

        // Insert regions into both pool and region table
        rt.insert(make_region_info(RegionId(1), RegionState::Shared(0)));
        rt.insert(make_region_info(RegionId(2), RegionState::Shared(0)));

        pool.insert(RegionId(1), 1024, 0).unwrap();
        pool.insert(RegionId(2), 1024, 0).unwrap();

        // Both should be Shared
        assert_eq!(rt.lookup(RegionId(1)).unwrap().state, RegionState::Shared(0));
        assert_eq!(rt.lookup(RegionId(2)).unwrap().state, RegionState::Shared(0));

        // Insert a third, evicting region 1
        pool.insert(RegionId(3), 1024, 0).unwrap();

        // Region 1 should now be Invalid in the region table
        assert_eq!(rt.lookup(RegionId(1)).unwrap().state, RegionState::Invalid);
        // Region 2 should still be Shared
        assert_eq!(rt.lookup(RegionId(2)).unwrap().state, RegionState::Shared(0));
    }

    #[test]
    fn test_cache_pool_zero_max_bytes() {
        // max_bytes=0 means no caching — all inserts fail immediately
        let pool = make_pool(0);
        let result = pool.insert(RegionId(1), 1024, 0);
        assert!(result.is_err());
        assert!(pool.lookup(RegionId(1)).is_none());
        assert_eq!(pool.used_bytes(), 0);
        assert_eq!(pool.len(), 0);
    }
}