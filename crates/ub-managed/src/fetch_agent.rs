use std::sync::Arc;

use parking_lot::Mutex;
use ub_core::types::{RegionId, RegionInfo, RegionState};

use crate::cache_pool::CachePool;
use crate::region::RegionTable;

/// FetchAgent — handles read-through cache logic for the managed layer.
///
/// When a read misses the local cache, the FetchAgent coordinates a FETCH
/// from the home node. Concurrent FETCH requests for the same region are
/// deduplicated — only one FETCH is sent, and other waiters are notified
/// when the data arrives.
pub struct FetchAgent {
    region_table: Arc<RegionTable>,
    cache_pool: Arc<CachePool>,
    /// In-flight FETCH requests, keyed by region_id.
    /// The Notify is signaled when the FETCH completes.
    in_flight: Mutex<Vec<RegionId>>,
}

impl FetchAgent {
    pub fn new(region_table: Arc<RegionTable>, cache_pool: Arc<CachePool>) -> Self {
        FetchAgent {
            region_table,
            cache_pool,
            in_flight: Mutex::new(Vec::new()),
        }
    }

    /// Read from a virtual address through the cache.
    ///
    /// - If the region is Home, read directly from the local MR (no cache needed).
    /// - If the region is Shared(epoch) and cached with matching epoch, read from cache (hit).
    /// - If the region is Shared but epoch mismatch (stale), auto-invalidate and return CacheMiss.
    /// - If the region is Invalid or not cached, trigger a FETCH (miss).
    ///
    /// Returns (cache_pool_offset, region_info) on hit, or None on miss
    /// (the caller should perform the actual FETCH and then call complete_fetch).
    pub fn read_va(&self, region_id: RegionId) -> ReadVaResult {
        let region = match self.region_table.lookup(region_id) {
            Some(r) => r,
            None => return ReadVaResult::UnknownRegion,
        };

        match region.state {
            RegionState::Home => ReadVaResult::Home(region),
            RegionState::Shared(region_epoch) => {
                // Check cache
                if let Some(entry) = self.cache_pool.lookup(region_id) {
                    // Epoch self-check: if cache entry epoch < region table epoch,
                    // the cached data is stale — auto-invalidate and re-fetch
                    if entry.epoch < region_epoch {
                        ub_obs::incr(ub_obs::REGION_CACHE_MISS);
                        self.invalidate_local(region_id);
                        // Re-lookup after invalidation
                        let region = self.region_table.lookup(region_id)
                            .expect("region must exist after invalidation");
                        return ReadVaResult::CacheMiss(region);
                    }
                    ub_obs::incr(ub_obs::REGION_CACHE_HIT);
                    ReadVaResult::CacheHit(entry.pool_offset, region)
                } else {
                    ub_obs::incr(ub_obs::REGION_CACHE_MISS);
                    ReadVaResult::CacheMiss(region)
                }
            }
            RegionState::Invalid => {
                ub_obs::incr(ub_obs::REGION_CACHE_MISS);
                ReadVaResult::CacheMiss(region)
            }
        }
    }

    /// Complete a FETCH operation — insert the data into the cache pool
    /// and update the region state to Shared.
    pub fn complete_fetch(&self, region_id: RegionId, len: u64, epoch: u64) -> Result<u64, String> {
        // Insert into cache pool
        let entry = self.cache_pool.insert(region_id, len, epoch)?;

        // Update region state to Shared(epoch)
        self.region_table.set_state(region_id, RegionState::Shared(epoch));
        self.region_table.set_epoch(region_id, epoch);

        // Remove from in-flight
        let mut in_flight = self.in_flight.lock();
        in_flight.retain(|id| *id != region_id);

        Ok(entry.pool_offset)
    }

    /// Mark a region as needing re-fetch (epoch check failed).
    /// Transitions the region to Invalid state.
    pub fn invalidate_local(&self, region_id: RegionId) -> bool {
        // Remove from cache
        self.cache_pool.remove(region_id);
        // Update state
        self.region_table.set_state(region_id, RegionState::Invalid)
    }

    /// Check if a FETCH is already in flight for this region.
    pub fn is_fetch_in_flight(&self, region_id: RegionId) -> bool {
        let in_flight = self.in_flight.lock();
        in_flight.contains(&region_id)
    }

    /// Mark a FETCH as in flight.
    pub fn mark_fetch_in_flight(&self, region_id: RegionId) {
        let mut in_flight = self.in_flight.lock();
        if !in_flight.contains(&region_id) {
            in_flight.push(region_id);
        }
    }

    /// Get cache pool reference.
    pub fn cache_pool(&self) -> &Arc<CachePool> {
        &self.cache_pool
    }

    /// Handle an INVALIDATE notification from the home node.
    ///
    /// Called when a remote writer has acquired write ownership and the home
    /// node broadcasts INVALIDATE to all replicas. The local node must
    /// invalidate its cached copy and transition the region to Invalid.
    ///
    /// Returns true if the region was successfully invalidated (was Shared).
    pub fn receive_invalidate(&self, region_id: RegionId, new_epoch: u64) -> bool {
        let region = match self.region_table.lookup(region_id) {
            Some(r) => r,
            None => return false,
        };

        match region.state {
            RegionState::Shared(current_epoch) => {
                // Invalidate if the new epoch is >= our current epoch.
                // Using >= (not >) because an INVALIDATE from the home node always
                // means the cached data is stale — epoch collisions can occur when
                // complete_fetch estimates the epoch or when acquire_writer doesn't
                // bump the epoch until release.
                if new_epoch >= current_epoch {
                    self.invalidate_local(region_id);
                    // Update the epoch so future reads know about the newer version
                    self.region_table.set_epoch(region_id, new_epoch);
                    ub_obs::incr(ub_obs::INVALIDATE_SENT);
                    true
                } else {
                    false // stale invalidate, ignore
                }
            }
            RegionState::Home => {
                // Home region should not receive INVALIDATE (we ARE the authority)
                false
            }
            RegionState::Invalid => {
                // Already invalid, just update epoch if newer
                if new_epoch > region.epoch {
                    self.region_table.set_epoch(region_id, new_epoch);
                }
                false
            }
        }
    }

    /// Check if a cached region's epoch is stale relative to a known latest epoch.
    /// Returns true if the region needs re-fetch.
    pub fn is_epoch_stale(&self, region_id: RegionId, latest_epoch: u64) -> bool {
        let region = match self.region_table.lookup(region_id) {
            Some(r) => r,
            None => return false,
        };

        match region.state {
            RegionState::Shared(epoch) => epoch < latest_epoch,
            _ => false,
        }
    }
}

/// Result of a read_va operation.
#[derive(Debug)]
pub enum ReadVaResult {
    /// Region is Home — read directly from the local MR.
    Home(RegionInfo),
    /// Cache hit — data is in the cache pool at the given offset.
    CacheHit(u64, RegionInfo),
    /// Cache miss — need to FETCH from the home node.
    CacheMiss(RegionInfo),
    /// Region not found in the local region table.
    UnknownRegion,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sub_alloc::SubAllocator;
    use ub_core::types::RegionInfo;

    fn make_fetch_agent() -> FetchAgent {
        let region_table = Arc::new(RegionTable::new());
        let cache_pool = Arc::new(CachePool::new(SubAllocator::new(0, 1024 * 1024), 1024 * 1024));
        FetchAgent::new(region_table, cache_pool)
    }

    fn make_region(region_id: u64, state: RegionState) -> RegionInfo {
        make_region_with_epoch(region_id, state, 0)
    }

    fn make_region_with_epoch(region_id: u64, state: RegionState, epoch: u64) -> RegionInfo {
        RegionInfo {
            region_id: RegionId(region_id),
            home_node_id: 1,
            device_id: 0,
            mr_handle: 1,
            base_offset: 0,
            len: 4096,
            epoch,
            state,
            local_mr_handle: None,
        }
    }

    #[test]
    fn test_fetch_agent_home_read() {
        let agent = make_fetch_agent();
        agent.region_table.insert(make_region(1, RegionState::Home));

        let result = agent.read_va(RegionId(1));
        match result {
            ReadVaResult::Home(info) => assert_eq!(info.region_id, RegionId(1)),
            _ => panic!("expected Home result"),
        }
    }

    #[test]
    fn test_fetch_agent_cache_miss() {
        let agent = make_fetch_agent();
        agent.region_table.insert(make_region(1, RegionState::Invalid));

        let result = agent.read_va(RegionId(1));
        match result {
            ReadVaResult::CacheMiss(info) => assert_eq!(info.region_id, RegionId(1)),
            _ => panic!("expected CacheMiss result"),
        }
    }

    #[test]
    fn test_fetch_agent_complete_fetch_then_hit() {
        let agent = make_fetch_agent();
        agent.region_table.insert(make_region(1, RegionState::Invalid));

        // First read → miss
        let result = agent.read_va(RegionId(1));
        assert!(matches!(result, ReadVaResult::CacheMiss(_)));

        // Complete the fetch
        let offset = agent.complete_fetch(RegionId(1), 4096, 1).unwrap();
        assert!(offset < u64::MAX); // just check it succeeded

        // Second read → hit
        let result = agent.read_va(RegionId(1));
        match result {
            ReadVaResult::CacheHit(off, info) => {
                assert_eq!(off, offset);
                assert_eq!(info.region_id, RegionId(1));
            }
            _ => panic!("expected CacheHit result"),
        }

        // Verify region state is now Shared
        let region = agent.region_table.lookup(RegionId(1)).unwrap();
        assert_eq!(region.state, RegionState::Shared(1));
    }

    #[test]
    fn test_fetch_agent_invalidate_local() {
        let agent = make_fetch_agent();
        agent.region_table.insert(make_region(1, RegionState::Shared(1)));
        agent.complete_fetch(RegionId(1), 4096, 1).unwrap();

        // Invalidate
        assert!(agent.invalidate_local(RegionId(1)));

        // Should be a miss now
        let result = agent.read_va(RegionId(1));
        assert!(matches!(result, ReadVaResult::CacheMiss(_)));

        // Region state should be Invalid
        let region = agent.region_table.lookup(RegionId(1)).unwrap();
        assert_eq!(region.state, RegionState::Invalid);
    }

    #[test]
    fn test_fetch_agent_unknown_region() {
        let agent = make_fetch_agent();
        let result = agent.read_va(RegionId(999));
        assert!(matches!(result, ReadVaResult::UnknownRegion));
    }

    #[test]
    fn test_fetch_agent_in_flight_tracking() {
        let agent = make_fetch_agent();
        assert!(!agent.is_fetch_in_flight(RegionId(1)));

        agent.mark_fetch_in_flight(RegionId(1));
        assert!(agent.is_fetch_in_flight(RegionId(1)));

        // Mark again (idempotent)
        agent.mark_fetch_in_flight(RegionId(1));
        assert!(agent.is_fetch_in_flight(RegionId(1)));

        // Complete fetch clears in-flight
        agent.region_table.insert(make_region(1, RegionState::Invalid));
        agent.complete_fetch(RegionId(1), 4096, 1).unwrap();
        assert!(!agent.is_fetch_in_flight(RegionId(1)));
    }

    #[test]
    fn test_fetch_agent_epoch_stale_auto_invalidate() {
        let agent = make_fetch_agent();
        agent.region_table.insert(make_region(1, RegionState::Shared(1)));
        agent.complete_fetch(RegionId(1), 4096, 1).unwrap();

        // Simulate epoch bump (e.g. another node wrote → invalidate received)
        // Manually bump the region table epoch past the cache entry epoch
        agent.region_table.set_epoch(RegionId(1), 5);
        agent.region_table.set_state(RegionId(1), RegionState::Shared(5));

        // read_va should detect stale cache, auto-invalidate, return CacheMiss
        let result = agent.read_va(RegionId(1));
        match result {
            ReadVaResult::CacheMiss(info) => {
                assert_eq!(info.region_id, RegionId(1));
            }
            _ => panic!("expected CacheMiss due to stale epoch, got {:?}", result),
        }

        // Region should now be Invalid
        let region = agent.region_table.lookup(RegionId(1)).unwrap();
        assert_eq!(region.state, RegionState::Invalid);
    }

    #[test]
    fn test_fetch_agent_receive_invalidate() {
        let agent = make_fetch_agent();
        agent.region_table.insert(make_region(1, RegionState::Shared(1)));
        agent.complete_fetch(RegionId(1), 4096, 1).unwrap();

        // Receive invalidate with newer epoch
        let result = agent.receive_invalidate(RegionId(1), 3);
        assert!(result);

        // Region should be Invalid now
        let region = agent.region_table.lookup(RegionId(1)).unwrap();
        assert_eq!(region.state, RegionState::Invalid);
        // Epoch should be updated to the new value
        assert_eq!(region.epoch, 3);
    }

    #[test]
    fn test_fetch_agent_receive_invalidate_stale() {
        let agent = make_fetch_agent();
        agent.region_table.insert(make_region(1, RegionState::Shared(5)));
        agent.complete_fetch(RegionId(1), 4096, 5).unwrap();

        // Receive invalidate with OLDER epoch — should be ignored
        let result = agent.receive_invalidate(RegionId(1), 2);
        assert!(!result);

        // Region should still be Shared(5)
        let region = agent.region_table.lookup(RegionId(1)).unwrap();
        assert_eq!(region.state, RegionState::Shared(5));
    }

    #[test]
    fn test_fetch_agent_receive_invalidate_home_region() {
        let agent = make_fetch_agent();
        agent.region_table.insert(make_region(1, RegionState::Home));

        // Home region should not accept INVALIDATE
        let result = agent.receive_invalidate(RegionId(1), 10);
        assert!(!result);

        // Region should still be Home
        let region = agent.region_table.lookup(RegionId(1)).unwrap();
        assert_eq!(region.state, RegionState::Home);
    }

    #[test]
    fn test_fetch_agent_receive_invalidate_unknown_region() {
        let agent = make_fetch_agent();

        let result = agent.receive_invalidate(RegionId(999), 1);
        assert!(!result);
    }

    #[test]
    fn test_fetch_agent_receive_invalidate_same_epoch() {
        // An INVALIDATE with the same epoch as the cached copy must still
        // invalidate the cache — the home node sent it, so the data is stale.
        let agent = make_fetch_agent();
        agent.region_table.insert(make_region(1, RegionState::Shared(1)));
        agent.complete_fetch(RegionId(1), 4096, 1).unwrap();

        // Receive invalidate with same epoch
        let result = agent.receive_invalidate(RegionId(1), 1);
        assert!(result);

        // Region should be Invalid now
        let region = agent.region_table.lookup(RegionId(1)).unwrap();
        assert_eq!(region.state, RegionState::Invalid);
    }

    #[test]
    fn test_fetch_agent_receive_invalidate_already_invalid() {
        let agent = make_fetch_agent();
        agent.region_table.insert(make_region_with_epoch(1, RegionState::Invalid, 2));

        // Invalid region receiving invalidate with newer epoch — should update epoch
        let result = agent.receive_invalidate(RegionId(1), 5);
        assert!(!result); // returns false since it wasn't Shared

        // But epoch should still be updated
        let region = agent.region_table.lookup(RegionId(1)).unwrap();
        assert_eq!(region.epoch, 5);
        assert_eq!(region.state, RegionState::Invalid);
    }

    #[test]
    fn test_fetch_agent_is_epoch_stale() {
        let agent = make_fetch_agent();
        agent.region_table.insert(make_region_with_epoch(1, RegionState::Shared(3), 3));

        assert!(agent.is_epoch_stale(RegionId(1), 5));  // stale
        assert!(!agent.is_epoch_stale(RegionId(1), 3)); // current
        assert!(!agent.is_epoch_stale(RegionId(1), 1)); // older (not stale)
        assert!(!agent.is_epoch_stale(RegionId(999), 1)); // unknown
    }

    #[test]
    fn test_fetch_agent_complete_fetch_updates_epoch() {
        let agent = make_fetch_agent();
        agent.region_table.insert(make_region_with_epoch(1, RegionState::Invalid, 0));

        // Complete fetch with epoch 5
        let offset = agent.complete_fetch(RegionId(1), 4096, 5).unwrap();

        // Region should be Shared(5)
        let region = agent.region_table.lookup(RegionId(1)).unwrap();
        assert_eq!(region.state, RegionState::Shared(5));
        assert_eq!(region.epoch, 5);

        // Cache entry should also have epoch 5
        let entry = agent.cache_pool.lookup(RegionId(1)).unwrap();
        assert_eq!(entry.epoch, 5);
        assert_eq!(entry.pool_offset, offset);
    }

    #[test]
    fn test_fetch_agent_cache_miss_then_fetch_then_hit_then_invalidate_then_miss() {
        // Full lifecycle test
        let agent = make_fetch_agent();
        agent.region_table.insert(make_region(1, RegionState::Invalid));

        // 1. Miss
        assert!(matches!(agent.read_va(RegionId(1)), ReadVaResult::CacheMiss(_)));

        // 2. Fetch complete
        agent.complete_fetch(RegionId(1), 4096, 1).unwrap();

        // 3. Hit
        assert!(matches!(agent.read_va(RegionId(1)), ReadVaResult::CacheHit(_, _)));

        // 4. Invalidate (simulating remote write)
        agent.receive_invalidate(RegionId(1), 2);

        // 5. Miss again
        assert!(matches!(agent.read_va(RegionId(1)), ReadVaResult::CacheMiss(_)));

        // 6. Re-fetch with new epoch
        agent.complete_fetch(RegionId(1), 4096, 2).unwrap();

        // 7. Hit with new epoch
        match agent.read_va(RegionId(1)) {
            ReadVaResult::CacheHit(_, info) => assert_eq!(info.epoch, 2),
            _ => panic!("expected CacheHit"),
        }
    }
}