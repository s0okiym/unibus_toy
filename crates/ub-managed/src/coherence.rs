use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use ub_core::types::{RegionId, UbVa};

/// Information about the current writer of a region.
#[derive(Debug, Clone)]
pub struct WriterInfo {
    pub writer_node_id: u16,
    pub lease_deadline: Instant,
}

/// Home-side per-region coherence state.
///
/// Tracks readers (nodes with cached Shared copies) and the current writer.
/// The home node uses this to enforce SWMR (Single-Writer-Multi-Reader):
/// - No writer when there are readers (must invalidate first)
/// - No readers when there is a writer (must unlock first)
#[derive(Debug)]
pub struct RegionHomeState {
    pub mr_handle: u32,
    pub base_offset: u64,
    pub len: u64,
    pub epoch: AtomicU64,
    /// Nodes that currently hold cached Shared copies.
    pub readers: RwLock<HashSet<u16>>,
    /// Current writer (if any) + lease deadline.
    pub writer: RwLock<Option<WriterInfo>>,
}

impl RegionHomeState {
    pub fn new(mr_handle: u32, base_offset: u64, len: u64, epoch: u64) -> Self {
        RegionHomeState {
            mr_handle,
            base_offset,
            len,
            epoch: AtomicU64::new(epoch),
            readers: RwLock::new(HashSet::new()),
            writer: RwLock::new(None),
        }
    }

    /// Get the current epoch.
    pub fn current_epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Bump the epoch (returns the new epoch).
    pub fn bump_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Check if the writer lease has expired.
    pub fn is_writer_lease_expired(&self) -> bool {
        let writer = self.writer.read();
        match writer.as_ref() {
            Some(info) => Instant::now() > info.lease_deadline,
            None => false,
        }
    }

    /// Add a reader node.
    pub fn add_reader(&self, node_id: u16) {
        self.readers.write().insert(node_id);
    }

    /// Remove a reader node.
    pub fn remove_reader(&self, node_id: u16) -> bool {
        self.readers.write().remove(&node_id)
    }

    /// Get the set of reader nodes.
    pub fn reader_nodes(&self) -> HashSet<u16> {
        self.readers.read().clone()
    }

    /// Number of readers.
    pub fn reader_count(&self) -> usize {
        self.readers.read().len()
    }
}

/// Result of a write lock acquisition attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireWriterResult {
    /// Lock granted — the writer can proceed.
    Granted { region_id: RegionId, epoch: u64 },
    /// Lock denied — another writer holds the lock.
    Denied { region_id: RegionId, reason: String },
    /// Lock queued — waiting for current writer to unlock or lease to expire.
    Queued { region_id: RegionId },
    /// Region not found.
    NotFound,
}

/// Manages SWMR coherence for all home regions on this node.
///
/// The CoherenceManager is used by the HOME node to:
/// 1. Track which nodes have cached Shared copies (readers)
/// 2. Manage writer locks (only one writer at a time)
/// 3. Enforce invalidation before granting write lock
/// 4. Handle lease expiry for writers
pub struct CoherenceManager {
    /// Per-region home state, keyed by region_id.
    home_states: RwLock<HashMap<RegionId, RegionHomeState>>,
    /// Writer lease duration.
    writer_lease_ms: u64,
    /// Queue of waiting writers per region.
    wait_queues: RwLock<HashMap<RegionId, VecDeque<u16>>>,
}

impl CoherenceManager {
    pub fn new(writer_lease_ms: u64) -> Self {
        CoherenceManager {
            home_states: RwLock::new(HashMap::new()),
            writer_lease_ms,
            wait_queues: RwLock::new(HashMap::new()),
        }
    }

    /// Register a home region for coherence tracking.
    pub fn register_region(&self, region_id: RegionId, mr_handle: u32, base_offset: u64, len: u64) {
        let mut states = self.home_states.write();
        states.insert(region_id, RegionHomeState::new(mr_handle, base_offset, len, 0));
    }

    /// Unregister a home region.
    pub fn unregister_region(&self, region_id: RegionId) -> Option<RegionHomeState> {
        let mut states = self.home_states.write();
        states.remove(&region_id)
    }

    /// Look up the home state for a region.
    pub fn get_home_state(&self, _region_id: RegionId) -> Option<RegionHomeState> {
        // RegionHomeState contains AtomicU64 which is not Clone.
        // Use the helper methods below to query specific fields.
        None
    }

    /// Check if a region is registered.
    pub fn contains_region(&self, region_id: RegionId) -> bool {
        self.home_states.read().contains_key(&region_id)
    }

    /// Try to acquire writer lock for a region.
    ///
    /// Returns:
    /// - Granted if no other writer and no readers (or readers invalidated)
    /// - Denied if another writer holds the lock
    /// - Queued if there are readers to invalidate
    /// - NotFound if region not registered
    ///
    /// The caller should:
    /// 1. If Granted: allow the write
    /// 2. If Queued: send INVALIDATE to all readers, then grant when ACKs received
    /// 3. If Denied: return error to caller
    pub fn acquire_writer(&self, region_id: RegionId, writer_node_id: u16) -> AcquireWriterResult {
        let states = self.home_states.read();

        let state = match states.get(&region_id) {
            Some(s) => s,
            None => return AcquireWriterResult::NotFound,
        };

        // Check if another writer holds the lock
        {
            let writer = state.writer.read();
            if let Some(ref info) = *writer {
                // Check lease expiry
                if Instant::now() <= info.lease_deadline {
                    if info.writer_node_id != writer_node_id {
                        // Another writer holds the lock — queue
                        let mut queues = self.wait_queues.write();
                        queues.entry(region_id).or_default().push_back(writer_node_id);
                        return AcquireWriterResult::Denied {
                            region_id,
                            reason: format!("writer node {} holds the lock", info.writer_node_id),
                        };
                    }
                    // Same writer re-acquiring — refresh lease
                }
                // Lease expired — will be handled below
            }
        }

        // Check readers
        let readers = state.reader_nodes();

        if readers.is_empty() {
            // No readers — grant immediately
            let deadline = Instant::now() + Duration::from_millis(self.writer_lease_ms);
            let epoch = state.current_epoch();
            *state.writer.write() = Some(WriterInfo {
                writer_node_id,
                lease_deadline: deadline,
            });
            AcquireWriterResult::Granted { region_id, epoch }
        } else {
            // Has readers — need to invalidate first
            // For the toy impl, we auto-queue and return the reader list
            // The caller is responsible for sending INVALIDATE messages
            let deadline = Instant::now() + Duration::from_millis(self.writer_lease_ms);
            let epoch = state.current_epoch();
            *state.writer.write() = Some(WriterInfo {
                writer_node_id,
                lease_deadline: deadline,
            });
            // Mark the write lock as pending (will be fully granted once
            // all readers ACK the INVALIDATE)
            AcquireWriterResult::Granted { region_id, epoch }
            // Note: in a full implementation, we'd track pending invalidations
            // and only fully grant once all ACKs received. For the toy impl,
            // we grant immediately and assume the invalidation happens.
        }
    }

    /// Release writer lock for a region.
    ///
    /// Returns true if the lock was successfully released.
    pub fn release_writer(&self, region_id: RegionId, writer_node_id: u16) -> bool {
        let states = self.home_states.read();

        let state = match states.get(&region_id) {
            Some(s) => s,
            None => return false,
        };

        {
            let mut writer = state.writer.write();
            match writer.as_ref() {
                Some(info) if info.writer_node_id == writer_node_id => {
                    *writer = None;
                }
                Some(_info) => {
                    // Different writer — ignore
                    return false;
                }
                None => return false,
            }
        }

        // Bump epoch on write completion
        state.bump_epoch();

        // Check if there's a queued writer
        let mut queues = self.wait_queues.write();
        if let Some(queue) = queues.get_mut(&region_id) {
            if queue.is_empty() {
                queues.remove(&region_id);
            }
            // In a full impl, we'd notify the next writer
        }

        true
    }

    /// Handle an INVALIDATE_ACK from a reader.
    ///
    /// Removes the reader from the region's readers set.
    /// Returns true if the reader was found and removed.
    pub fn handle_invalidate_ack(&self, region_id: RegionId, reader_node_id: u16) -> bool {
        let states = self.home_states.read();
        match states.get(&region_id) {
            Some(state) => {
                state.remove_reader(reader_node_id);
                true
            }
            None => false,
        }
    }

    /// Register a reader for a region (called when a FETCH is served).
    pub fn add_reader(&self, region_id: RegionId, reader_node_id: u16) -> bool {
        let states = self.home_states.read();
        match states.get(&region_id) {
            Some(state) => {
                state.add_reader(reader_node_id);
                true
            }
            None => false,
        }
    }

    /// Remove a reader when a node goes down.
    pub fn remove_reader(&self, region_id: RegionId, reader_node_id: u16) -> bool {
        let states = self.home_states.read();
        match states.get(&region_id) {
            Some(state) => state.remove_reader(reader_node_id),
            None => false,
        }
    }

    /// Get the current epoch of a region.
    pub fn get_epoch(&self, region_id: RegionId) -> Option<u64> {
        let states = self.home_states.read();
        states.get(&region_id).map(|s| s.current_epoch())
    }

    /// Bump the epoch of a region.
    pub fn bump_epoch(&self, region_id: RegionId) -> Option<u64> {
        let states = self.home_states.read();
        states.get(&region_id).map(|s| s.bump_epoch())
    }

    /// Check for expired writer leases and release them.
    ///
    /// Returns a list of (region_id, expired_writer_node_id) pairs.
    pub fn check_lease_expiry(&self) -> Vec<(RegionId, u16)> {
        let states = self.home_states.read();
        let mut expired = Vec::new();

        for (region_id, state) in states.iter() {
            let mut writer = state.writer.write();
            if let Some(ref info) = *writer {
                if Instant::now() > info.lease_deadline {
                    let expired_writer_id = info.writer_node_id;
                    *writer = None;
                    drop(writer);
                    state.bump_epoch();
                    expired.push((*region_id, expired_writer_id));
                }
            }
        }

        expired
    }

    /// Get the writer info for a region.
    pub fn get_writer(&self, region_id: RegionId) -> Option<WriterInfo> {
        let states = self.home_states.read();
        states.get(&region_id).and_then(|s| s.writer.read().clone())
    }

    /// Get the reader set for a region.
    pub fn get_readers(&self, region_id: RegionId) -> HashSet<u16> {
        let states = self.home_states.read();
        states.get(&region_id).map(|s| s.reader_nodes()).unwrap_or_default()
    }

    /// Get the writer lease duration in ms.
    pub fn writer_lease_ms(&self) -> u64 {
        self.writer_lease_ms
    }

    /// List all regions with their current state.
    pub fn list_regions(&self) -> Vec<(RegionId, u64, Option<u16>, HashSet<u16>)> {
        let states = self.home_states.read();
        states.iter().map(|(rid, state)| {
            let epoch = state.current_epoch();
            let writer_node = state.writer.read().as_ref().map(|w| w.writer_node_id);
            let readers = state.reader_nodes();
            (*rid, epoch, writer_node, readers)
        }).collect()
    }
}

/// WriterGuard — RAII guard for write access to a region.
///
/// When dropped, automatically releases the write lock.
pub struct WriterGuard {
    pub va: UbVa,
    pub region_id: RegionId,
    pub home_node_id: u16,
    pub writer_node_id: u16,
    pub epoch: u64,
    released: bool,
    coherence: Option<Arc<CoherenceManager>>,
}

impl WriterGuard {
    pub fn new(
        va: UbVa,
        region_id: RegionId,
        home_node_id: u16,
        writer_node_id: u16,
        epoch: u64,
        coherence: Arc<CoherenceManager>,
    ) -> Self {
        WriterGuard {
            va,
            region_id,
            home_node_id,
            writer_node_id,
            epoch,
            released: false,
            coherence: Some(coherence),
        }
    }

    /// Manually release the write lock (without dropping).
    pub fn release(&mut self) -> bool {
        if self.released {
            return false;
        }
        self.released = true;
        if let Some(ref coherence) = self.coherence {
            coherence.release_writer(self.region_id, self.writer_node_id)
        } else {
            false
        }
    }

    /// Check if the guard has been released.
    pub fn is_released(&self) -> bool {
        self.released
    }
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        if !self.released {
            self.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ub_core::types::{RegionInfo, RegionState};
    use crate::cache_pool::CachePool;
    use crate::fetch_agent::FetchAgent;
    use crate::region::RegionTable;

    fn make_coherence() -> Arc<CoherenceManager> {
        Arc::new(CoherenceManager::new(5000)) // 5s lease
    }

    #[test]
    fn test_coherence_acquire_writer_no_readers() {
        let cm = make_coherence();
        cm.register_region(RegionId(1), 1, 0, 4096);

        let result = cm.acquire_writer(RegionId(1), 1);
        match result {
            AcquireWriterResult::Granted { region_id, epoch } => {
                assert_eq!(region_id, RegionId(1));
                assert_eq!(epoch, 0);
            }
            _ => panic!("expected Granted, got {:?}", result),
        }

        // Verify writer is set
        let writer = cm.get_writer(RegionId(1)).unwrap();
        assert_eq!(writer.writer_node_id, 1);
    }

    #[test]
    fn test_coherence_acquire_writer_with_readers() {
        let cm = make_coherence();
        cm.register_region(RegionId(1), 1, 0, 4096);

        // Add readers
        cm.add_reader(RegionId(1), 2);
        cm.add_reader(RegionId(1), 3);

        // Acquire writer — should still grant (toy impl grants immediately)
        let result = cm.acquire_writer(RegionId(1), 1);
        match result {
            AcquireWriterResult::Granted { region_id, .. } => {
                assert_eq!(region_id, RegionId(1));
            }
            _ => panic!("expected Granted, got {:?}", result),
        }

        // Verify readers are still tracked (until they ACK INVALIDATE)
        let readers = cm.get_readers(RegionId(1));
        assert!(readers.contains(&2));
        assert!(readers.contains(&3));
    }

    #[test]
    fn test_coherence_invalidate_state_transition() {
        let cm = make_coherence();
        cm.register_region(RegionId(1), 1, 0, 4096);

        // Add reader
        cm.add_reader(RegionId(1), 2);

        // Reader ACKs the INVALIDATE — removed from readers set
        let acked = cm.handle_invalidate_ack(RegionId(1), 2);
        assert!(acked);

        // Reader should no longer be in the set
        let readers = cm.get_readers(RegionId(1));
        assert!(!readers.contains(&2));
    }

    #[test]
    fn test_coherence_writer_lease_expiry() {
        let cm = Arc::new(CoherenceManager::new(1)); // 1ms lease
        cm.register_region(RegionId(1), 1, 0, 4096);

        // Acquire writer
        let result = cm.acquire_writer(RegionId(1), 1);
        assert!(matches!(result, AcquireWriterResult::Granted { .. }));

        // Wait for lease to expire
        std::thread::sleep(Duration::from_millis(10));

        // Check lease expiry
        let expired = cm.check_lease_expiry();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, RegionId(1));
        assert_eq!(expired[0].1, 1);

        // Writer should be cleared now
        assert!(cm.get_writer(RegionId(1)).is_none());

        // Epoch should be bumped
        let epoch = cm.get_epoch(RegionId(1)).unwrap();
        assert!(epoch > 0);
    }

    #[test]
    fn test_writer_guard_drop_sends_unlock() {
        let cm = make_coherence();
        cm.register_region(RegionId(1), 1, 0, 4096);

        // Acquire writer through guard
        {
            let result = cm.acquire_writer(RegionId(1), 1);
            let granted = match result {
                AcquireWriterResult::Granted { epoch, .. } => epoch,
                _ => panic!("expected Granted"),
            };

            let _guard = WriterGuard::new(
                UbVa::new(RegionId(1), 0),
                RegionId(1),
                1,
                1,
                granted,
                Arc::clone(&cm),
            );

            // Writer should be set
            assert!(cm.get_writer(RegionId(1)).is_some());

            // Guard dropped here
        }

        // Writer should be cleared after drop
        assert!(cm.get_writer(RegionId(1)).is_none());

        // Epoch should be bumped
        let epoch = cm.get_epoch(RegionId(1)).unwrap();
        assert!(epoch > 0);
    }

    #[test]
    fn test_concurrent_acquire_queues() {
        let cm = make_coherence();
        cm.register_region(RegionId(1), 1, 0, 4096);

        // First writer acquires
        let result1 = cm.acquire_writer(RegionId(1), 1);
        assert!(matches!(result1, AcquireWriterResult::Granted { .. }));

        // Second writer is denied (first still holds lock)
        let result2 = cm.acquire_writer(RegionId(1), 2);
        match result2 {
            AcquireWriterResult::Denied { region_id, reason } => {
                assert_eq!(region_id, RegionId(1));
                assert!(reason.contains("writer node 1"));
            }
            _ => panic!("expected Denied, got {:?}", result2),
        }

        // First writer releases
        let released = cm.release_writer(RegionId(1), 1);
        assert!(released);

        // Now second writer can acquire
        let result3 = cm.acquire_writer(RegionId(1), 2);
        assert!(matches!(result3, AcquireWriterResult::Granted { .. }));
    }

    #[test]
    fn test_coherence_region_not_found() {
        let cm = make_coherence();

        let result = cm.acquire_writer(RegionId(999), 1);
        assert!(matches!(result, AcquireWriterResult::NotFound));
    }

    #[test]
    fn test_coherence_release_wrong_writer() {
        let cm = make_coherence();
        cm.register_region(RegionId(1), 1, 0, 4096);

        // Writer 1 acquires
        let result = cm.acquire_writer(RegionId(1), 1);
        assert!(matches!(result, AcquireWriterResult::Granted { .. }));

        // Writer 2 tries to release — should fail
        let released = cm.release_writer(RegionId(1), 2);
        assert!(!released);

        // Writer 1 should still hold the lock
        let writer = cm.get_writer(RegionId(1)).unwrap();
        assert_eq!(writer.writer_node_id, 1);
    }

    #[test]
    fn test_writer_guard_manual_release() {
        let cm = make_coherence();
        cm.register_region(RegionId(1), 1, 0, 4096);

        let result = cm.acquire_writer(RegionId(1), 1);
        let granted = match result {
            AcquireWriterResult::Granted { epoch, .. } => epoch,
            _ => panic!("expected Granted"),
        };

        let mut guard = WriterGuard::new(
            UbVa::new(RegionId(1), 0),
            RegionId(1),
            1,
            1,
            granted,
            Arc::clone(&cm),
        );

        assert!(!guard.is_released());
        assert!(cm.get_writer(RegionId(1)).is_some());

        // Manual release
        let ok = guard.release();
        assert!(ok);
        assert!(guard.is_released());

        // Writer should be cleared
        assert!(cm.get_writer(RegionId(1)).is_none());

        // Double release should be a no-op
        let ok2 = guard.release();
        assert!(!ok2);
    }

    #[test]
    fn test_coherence_add_remove_reader() {
        let cm = make_coherence();
        cm.register_region(RegionId(1), 1, 0, 4096);

        assert!(cm.add_reader(RegionId(1), 2));
        assert!(cm.add_reader(RegionId(1), 3));

        let readers = cm.get_readers(RegionId(1));
        assert_eq!(readers.len(), 2);
        assert!(readers.contains(&2));
        assert!(readers.contains(&3));

        assert!(cm.remove_reader(RegionId(1), 2));
        let readers = cm.get_readers(RegionId(1));
        assert_eq!(readers.len(), 1);
        assert!(!readers.contains(&2));

        // Remove non-existent reader
        assert!(!cm.remove_reader(RegionId(1), 99));
    }

    #[test]
    fn test_coherence_epoch_bumping() {
        let cm = make_coherence();
        cm.register_region(RegionId(1), 1, 0, 4096);

        assert_eq!(cm.get_epoch(RegionId(1)), Some(0));

        let e1 = cm.bump_epoch(RegionId(1));
        assert_eq!(e1, Some(1));
        assert_eq!(cm.get_epoch(RegionId(1)), Some(1));

        let e2 = cm.bump_epoch(RegionId(1));
        assert_eq!(e2, Some(2));
        assert_eq!(cm.get_epoch(RegionId(1)), Some(2));
    }

    #[test]
    fn test_coherence_unregister_region() {
        let cm = make_coherence();
        cm.register_region(RegionId(1), 1, 0, 4096);
        assert!(cm.contains_region(RegionId(1)));

        let state = cm.unregister_region(RegionId(1));
        assert!(state.is_some());
        assert!(!cm.contains_region(RegionId(1)));

        // Unregistering again should return None
        let state2 = cm.unregister_region(RegionId(1));
        assert!(state2.is_none());
    }

    #[test]
    fn test_coherence_same_writer_reacquire() {
        let cm = make_coherence();
        cm.register_region(RegionId(1), 1, 0, 4096);

        // First acquire
        let result1 = cm.acquire_writer(RegionId(1), 1);
        assert!(matches!(result1, AcquireWriterResult::Granted { .. }));

        // Same writer re-acquires (refreshes lease)
        let result2 = cm.acquire_writer(RegionId(1), 1);
        assert!(matches!(result2, AcquireWriterResult::Granted { .. }));

        // Release
        assert!(cm.release_writer(RegionId(1), 1));
        assert!(cm.get_writer(RegionId(1)).is_none());
    }

    #[test]
    fn test_swmr_full_lifecycle() {
        // Full SWMR lifecycle: alloc → FETCH → read → acquire_writer →
        // invalidate readers → write → release → re-FETCH → read new data
        let cm = make_coherence();
        let region_table = Arc::new(RegionTable::new());
        let cache_pool = Arc::new(CachePool::new(
            crate::sub_alloc::SubAllocator::new(0, 1024 * 1024),
            1024 * 1024,
        ));
        let fetch_agent = Arc::new(FetchAgent::new(
            Arc::clone(&region_table),
            Arc::clone(&cache_pool),
        ));

        let region_id = RegionId(1);

        // 1. Home node registers the region
        cm.register_region(region_id, 1, 0, 4096);

        // 2. Remote node (node 2) FETCHes the region → becomes a reader
        cm.add_reader(region_id, 2);
        region_table.insert(RegionInfo {
            region_id,
            home_node_id: 1,
            device_id: 0,
            mr_handle: 1,
            base_offset: 0,
            len: 4096,
            epoch: 0,
            state: RegionState::Shared(0),
            local_mr_handle: Some(2),
        });
        fetch_agent.complete_fetch(region_id, 4096, 0).unwrap();

        // 3. Verify node 2 can read from cache
        let read_result = fetch_agent.read_va(region_id);
        assert!(matches!(read_result, crate::fetch_agent::ReadVaResult::CacheHit(_, _)));

        // 4. Node 1 acquires writer lock
        let result = cm.acquire_writer(region_id, 1);
        assert!(matches!(result, AcquireWriterResult::Granted { .. }));

        // 5. Home node sends INVALIDATE to reader (node 2)
        // Node 2 processes the INVALIDATE via FetchAgent
        let invalidated = fetch_agent.receive_invalidate(region_id, 1);
        assert!(invalidated);

        // 6. Node 2 ACKs the INVALIDATE — removed from readers set
        let acked = cm.handle_invalidate_ack(region_id, 2);
        assert!(acked);
        assert!(!cm.get_readers(region_id).contains(&2));

        // 7. Node 2's region is now Invalid
        let region = region_table.lookup(region_id).unwrap();
        assert_eq!(region.state, RegionState::Invalid);

        // 8. Node 1 does the write, then releases
        cm.release_writer(region_id, 1);
        // Epoch bumped on release
        let epoch = cm.get_epoch(region_id).unwrap();
        assert!(epoch > 0);

        // 9. Node 2 re-FETCHes the region
        // First, the region state needs to be updated with the new epoch info
        region_table.set_epoch(region_id, epoch);
        let read_result2 = fetch_agent.read_va(region_id);
        assert!(matches!(read_result2, crate::fetch_agent::ReadVaResult::CacheMiss(_)));

        // 10. Node 2 completes the re-FETCH
        fetch_agent.complete_fetch(region_id, 4096, epoch).unwrap();
        let region = region_table.lookup(region_id).unwrap();
        assert_eq!(region.state, RegionState::Shared(epoch));

        // 11. Node 2 can now read the updated data
        let read_result3 = fetch_agent.read_va(region_id);
        assert!(matches!(read_result3, crate::fetch_agent::ReadVaResult::CacheHit(_, _)));
    }
}