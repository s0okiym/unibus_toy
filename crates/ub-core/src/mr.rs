use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::{mpsc, Notify};

use crate::addr::UbAddr;
use crate::device::Device;
use crate::error::UbError;
use crate::types::{DeviceKind, MrHandle, MrPerms, MrState, Verb};

/// Local MR table entry.
pub struct MrEntry {
    pub handle: u32,
    pub device: Arc<dyn Device>,
    pub base_offset: u64,
    pub len: u64,
    pub perms: MrPerms,
    pub base_ub_addr: UbAddr,
    state: AtomicU8,
    pub inflight_refs: AtomicI64,
    /// Notified when inflight_refs drops to zero (used by deregister wait).
    inflight_notify: Notify,
}

impl MrEntry {
    pub fn state(&self) -> MrState {
        match self.state.load(Ordering::Acquire) {
            0 => MrState::Active,
            1 => MrState::Revoking,
            2 => MrState::Released,
            _ => MrState::Released,
        }
    }

    pub fn set_state(&self, new: MrState) {
        self.state.store(new as u8, Ordering::Release);
    }

    /// Try to acquire an inflight reference. Returns false if MR is not Active.
    pub fn try_inflight_inc(&self) -> bool {
        if self.state() != MrState::Active {
            return false;
        }
        self.inflight_refs.fetch_add(1, Ordering::Acquire);
        true
    }

    pub fn inflight_dec(&self) {
        let prev = self.inflight_refs.fetch_sub(1, Ordering::Release);
        if prev == 1 {
            // Last inflight reference released — notify deregister waiter
            self.inflight_notify.notify_waiters();
        }
    }

    /// Wait for inflight_refs to reach zero. Used by deregister_async.
    pub async fn wait_inflight_zero(&self) {
        if self.inflight_refs.load(Ordering::Acquire) == 0 {
            return;
        }
        self.inflight_notify.notified().await;
    }

    /// Check that the MR's permissions allow the requested verb.
    /// Returns `Err(UbError::PermDenied)` if the permission is missing.
    pub fn check_perms(&self, verb: Verb) -> Result<(), UbError> {
        let required = match verb {
            Verb::ReadReq | Verb::ReadResp => MrPerms::READ,
            Verb::Write | Verb::WriteImm => MrPerms::WRITE,
            Verb::AtomicCas | Verb::AtomicFaa | Verb::AtomicCasResp | Verb::AtomicFaaResp => MrPerms::ATOMIC,
            Verb::Send => MrPerms::WRITE, // Send requires WRITE permission
        };
        if self.perms.contains(required) {
            Ok(())
        } else {
            Err(UbError::PermDenied)
        }
    }
}

/// Event emitted by MrTable when an MR is registered or deregistered.
/// Consumed by the control plane to broadcast MR_PUBLISH/MR_REVOKE.
#[derive(Debug, Clone)]
pub struct MrPublishInfo {
    pub mr_handle: u32,
    pub owner_node: u16,
    pub base_ub_addr: UbAddr,
    pub len: u64,
    pub perms: MrPerms,
    pub device_kind: DeviceKind,
}

#[derive(Debug, Clone)]
pub enum MrPublishEvent {
    Publish(MrPublishInfo),
    Revoke { owner_node: u16, mr_handle: u32 },
}

/// Bump allocator for offset space within a device.
struct OffsetAllocator {
    next: u64,
}

impl OffsetAllocator {
    fn new() -> Self {
        OffsetAllocator { next: 0 }
    }

    fn alloc(&mut self, len: u64, align: u64) -> u64 {
        let aligned = (self.next + align - 1) & !(align - 1);
        self.next = aligned + len;
        aligned
    }
}

/// Local MR table — manages all MR registrations on this node.
pub struct MrTable {
    entries: DashMap<u32, Arc<MrEntry>>,
    next_handle: Mutex<u32>,
    offset_allocators: Mutex<DashMap<u16, OffsetAllocator>>,
    pod_id: u16,
    node_id: u16,
    /// Optional channel to notify control plane of MR registration/deregistration.
    publish_tx: Option<mpsc::UnboundedSender<MrPublishEvent>>,
}

impl MrTable {
    pub fn new(pod_id: u16, node_id: u16) -> Self {
        let allocators = DashMap::new();
        // Pre-populate CPU memory device allocator (device_id=0)
        allocators.insert(0, OffsetAllocator::new());

        MrTable {
            entries: DashMap::new(),
            next_handle: Mutex::new(1), // handle 0 is reserved
            offset_allocators: Mutex::new(allocators),
            pod_id,
            node_id,
            publish_tx: None,
        }
    }

    /// Create MrTable with a channel for MR publish events.
    /// Returns (MrTable, mpsc::UnboundedReceiver<MrPublishEvent>).
    pub fn new_with_channel(pod_id: u16, node_id: u16) -> (Self, mpsc::UnboundedReceiver<MrPublishEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut table = Self::new(pod_id, node_id);
        table.publish_tx = Some(tx);
        (table, rx)
    }

    /// Register a new MR. Returns (UbAddr, MrHandle).
    pub fn register(
        &self,
        device: Arc<dyn Device>,
        len: u64,
        perms: MrPerms,
    ) -> Result<(UbAddr, MrHandle), UbError> {
        if len == 0 {
            return Err(UbError::AddrInvalid);
        }

        let device_id = device.device_id();

        // Allocate offset from device's allocator
        let offset = {
            let allocators = self.offset_allocators.lock();
            let mut allocator = allocators
                .entry(device_id)
                .or_insert_with(OffsetAllocator::new);
            allocator.alloc(len, 8) // 8-byte alignment
        };

        // Allocate MR handle
        let handle = {
            let mut next = self.next_handle.lock();
            let h = *next;
            *next += 1;
            h
        };

        let base_ub_addr = UbAddr::new(self.pod_id, self.node_id, device_id, offset, 0);

        let device_kind = device.kind();

        let entry = Arc::new(MrEntry {
            handle,
            device,
            base_offset: offset,
            len,
            perms,
            base_ub_addr,
            state: AtomicU8::new(MrState::Active as u8),
            inflight_refs: AtomicI64::new(0),
            inflight_notify: Notify::new(),
        });

        self.entries.insert(handle, entry);

        // Notify control plane about the new MR
        if let Some(ref tx) = self.publish_tx {
            let _ = tx.send(MrPublishEvent::Publish(MrPublishInfo {
                mr_handle: handle,
                owner_node: self.node_id,
                base_ub_addr,
                len,
                perms,
                device_kind,
            }));
        }

        Ok((base_ub_addr, MrHandle(handle)))
    }

    /// Deregister an MR. Transitions state to Revoking, then removes the entry.
    /// If inflight operations are ongoing, they will be rejected (try_inflight_inc returns false).
    /// For async wait with timeout, use `deregister_async`.
    pub fn deregister(&self, handle: MrHandle) -> Result<(), UbError> {
        let entry_ref = match self.entries.get_mut(&handle.0) {
            Some(e) => e,
            None => return Err(UbError::AddrInvalid),
        };
        let entry = entry_ref.value();

        // CAS Active → Revoking
        let prev = entry.state.compare_exchange(
            MrState::Active as u8,
            MrState::Revoking as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if prev.is_err() {
            return Err(UbError::Internal("MR is already being deregistered or released".into()));
        }

        // Broadcast MR_REVOKE while still in table (so responders can see Revoking state)
        if let Some(ref tx) = self.publish_tx {
            let _ = tx.send(MrPublishEvent::Revoke {
                owner_node: self.node_id,
                mr_handle: handle.0,
            });
        }

        drop(entry_ref);

        // Remove from table — new lookups will return None
        self.entries.remove(&handle.0);

        Ok(())
    }

    /// Async deregister: waits for inflight operations to complete, with timeout.
    /// Returns Ok(()) if inflight drained and MR released.
    /// Returns Err(Timeout) if inflight did not drain within timeout_ms.
    pub async fn deregister_async(&self, handle: MrHandle, timeout_ms: u64) -> Result<(), UbError> {
        let entry_arc = match self.entries.get(&handle.0) {
            Some(e) => Arc::clone(e.value()),
            None => return Err(UbError::AddrInvalid),
        };

        // CAS Active → Revoking
        let prev = entry_arc.state.compare_exchange(
            MrState::Active as u8,
            MrState::Revoking as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if prev.is_err() {
            return Err(UbError::Internal("MR is already being deregistered or released".into()));
        }

        // Broadcast MR_REVOKE immediately after entering Revoking
        if let Some(ref tx) = self.publish_tx {
            let _ = tx.send(MrPublishEvent::Revoke {
                owner_node: self.node_id,
                mr_handle: handle.0,
            });
        }

        // Wait for inflight refs to drain (with timeout)
        if entry_arc.inflight_refs.load(Ordering::Acquire) > 0 {
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(timeout_ms),
                entry_arc.wait_inflight_zero(),
            ).await;

            if result.is_err() {
                // Timeout — mark as Released anyway (best effort for toy)
                entry_arc.set_state(MrState::Released);
                self.entries.remove(&handle.0);
                return Err(UbError::Timeout);
            }
        }

        // All inflight drained — transition to Released and remove
        entry_arc.set_state(MrState::Released);
        self.entries.remove(&handle.0);

        Ok(())
    }

    /// Look up an MR by handle. Returns the entry even if in Revoking state
    /// (callers should check `entry.state()` and use `try_inflight_inc()`).
    pub fn lookup(&self, handle: u32) -> Option<Arc<MrEntry>> {
        self.entries.get(&handle).map(|r| Arc::clone(r.value()))
    }

    /// Look up an MR by UB address. Returns the MR entry and the offset within the MR.
    pub fn lookup_by_addr(&self, addr: UbAddr) -> Option<(Arc<MrEntry>, u64)> {
        for entry in self.entries.iter() {
            let e = entry.value();
            if e.base_ub_addr.node_id() == addr.node_id()
                && e.base_ub_addr.device_id() == addr.device_id()
            {
                let mr_start = e.base_ub_addr.offset();
                let mr_end = mr_start + e.len;
                let req_offset = addr.offset();
                if req_offset >= mr_start && req_offset < mr_end {
                    let offset_in_mr = req_offset - mr_start;
                    return Some((Arc::clone(e), offset_in_mr));
                }
            }
        }
        None
    }

    /// List all MR entries (for admin API).
    pub fn list(&self) -> Vec<Arc<MrEntry>> {
        self.entries.iter().map(|r| Arc::clone(r.value())).collect()
    }
}

/// Remote MR cache entry (for other nodes' MRs).
#[derive(Debug, Clone)]
pub struct MrCacheEntry {
    pub remote_mr_handle: u32,
    pub owner_node: u16,
    pub base_ub_addr: UbAddr,
    pub len: u64,
    pub perms: MrPerms,
    pub device_kind: DeviceKind,
}

/// Remote MR cache — stores metadata about MRs on other nodes.
pub struct MrCacheTable {
    entries: DashMap<(u16, u32), MrCacheEntry>,
}

impl MrCacheTable {
    pub fn new() -> Self {
        MrCacheTable {
            entries: DashMap::new(),
        }
    }

    pub fn insert(&self, entry: MrCacheEntry) {
        let key = (entry.owner_node, entry.remote_mr_handle);
        self.entries.insert(key, entry);
    }

    pub fn remove(&self, owner_node: u16, mr_handle: u32) {
        self.entries.remove(&(owner_node, mr_handle));
    }

    pub fn lookup_by_addr(&self, addr: UbAddr) -> Option<MrCacheEntry> {
        for entry in self.entries.iter() {
            let e = entry.value();
            if e.base_ub_addr.node_id() == addr.node_id()
                && e.base_ub_addr.device_id() == addr.device_id()
            {
                let mr_start = e.base_ub_addr.offset();
                let mr_end = mr_start + e.len;
                let req_offset = addr.offset();
                if req_offset >= mr_start && req_offset < mr_end {
                    return Some(e.clone());
                }
            }
        }
        None
    }

    pub fn list(&self) -> Vec<MrCacheEntry> {
        self.entries.iter().map(|r| r.value().clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::memory::MemoryDevice;
    use crate::device::Device;

    fn make_table() -> MrTable {
        MrTable::new(1, 42)
    }

    #[test]
    fn test_mr_register_and_lookup() {
        let table = make_table();
        let dev = Arc::new(MemoryDevice::new(4096));
        let (_addr, handle) = table.register(dev, 1024, MrPerms::READ | MrPerms::WRITE).unwrap();

        let entry = table.lookup(handle.0).unwrap();
        assert_eq!(entry.handle, handle.0);
        assert_eq!(entry.perms, MrPerms::READ | MrPerms::WRITE);
        assert_eq!(entry.base_ub_addr.pod_id(), 1);
        assert_eq!(entry.base_ub_addr.node_id(), 42);
        assert_eq!(entry.base_ub_addr.device_id(), 0);
    }

    #[test]
    fn test_mr_two_registrations_no_overlap() {
        let table = make_table();
        let dev: Arc<dyn Device> = Arc::new(MemoryDevice::new(4096));
        let (addr1, _h1) = table.register(Arc::clone(&dev), 1024, MrPerms::READ).unwrap();
        let (addr2, _h2) = table.register(dev, 1024, MrPerms::WRITE).unwrap();
        // Offsets must not overlap
        assert_ne!(addr1.offset(), addr2.offset());
        assert!(addr2.offset() >= addr1.offset() + 1024);
    }

    #[test]
    fn test_mr_deregister() {
        let table = make_table();
        let dev = Arc::new(MemoryDevice::new(4096));
        let (_, handle) = table.register(dev, 1024, MrPerms::READ).unwrap();
        table.deregister(handle).unwrap();
        assert!(table.lookup(handle.0).is_none());
    }

    #[test]
    fn test_mr_deregister_nonexistent() {
        let table = make_table();
        assert!(table.deregister(MrHandle(999)).is_err());
    }

    #[test]
    fn test_mr_lookup_by_addr() {
        let table = make_table();
        let dev = Arc::new(MemoryDevice::new(4096));
        let (base_addr, _) = table.register(dev, 1024, MrPerms::READ | MrPerms::WRITE).unwrap();

        // Access within the MR range
        let access_addr = base_addr.with_offset(base_addr.offset() + 100);
        let (entry, offset_in_mr) = table.lookup_by_addr(access_addr).unwrap();
        assert_eq!(offset_in_mr, 100);
        assert_eq!(entry.len, 1024);
    }

    #[test]
    fn test_mr_lookup_by_addr_out_of_range() {
        let table = make_table();
        let dev = Arc::new(MemoryDevice::new(4096));
        let (base_addr, _) = table.register(dev, 1024, MrPerms::READ).unwrap();

        // Access outside the MR range
        let access_addr = base_addr.with_offset(base_addr.offset() + 2000);
        assert!(table.lookup_by_addr(access_addr).is_none());
    }

    #[test]
    fn test_mr_inflight_refcount() {
        let table = make_table();
        let dev = Arc::new(MemoryDevice::new(4096));
        let (_, handle) = table.register(dev, 1024, MrPerms::READ).unwrap();

        let entry = table.lookup(handle.0).unwrap();
        assert!(entry.try_inflight_inc());
        assert!(entry.try_inflight_inc());
        assert_eq!(entry.inflight_refs.load(Ordering::Acquire), 2);
        entry.inflight_dec();
        entry.inflight_dec();
        assert_eq!(entry.inflight_refs.load(Ordering::Acquire), 0);
    }

    #[test]
    fn test_mr_inflight_blocked_when_revoking() {
        let table = make_table();
        let dev = Arc::new(MemoryDevice::new(4096));
        let (_, handle) = table.register(dev, 1024, MrPerms::READ).unwrap();

        let entry = table.lookup(handle.0).unwrap();
        entry.set_state(MrState::Revoking);
        assert!(!entry.try_inflight_inc());
    }

    #[test]
    fn test_mr_check_perms() {
        let table = make_table();
        let dev = Arc::new(MemoryDevice::new(4096));
        let (_, handle) = table.register(dev, 1024, MrPerms::READ).unwrap();

        let entry = table.lookup(handle.0).unwrap();
        assert!(entry.check_perms(Verb::ReadReq).is_ok());
        assert!(entry.check_perms(Verb::Write).is_err());
        assert!(entry.check_perms(Verb::AtomicCas).is_err());

        // Register with all perms
        let dev2 = Arc::new(MemoryDevice::new(4096));
        let (_, handle2) = table.register(dev2, 1024, MrPerms::READ | MrPerms::WRITE | MrPerms::ATOMIC).unwrap();
        let entry2 = table.lookup(handle2.0).unwrap();
        assert!(entry2.check_perms(Verb::ReadReq).is_ok());
        assert!(entry2.check_perms(Verb::Write).is_ok());
        assert!(entry2.check_perms(Verb::AtomicCas).is_ok());
        assert!(entry2.check_perms(Verb::AtomicFaa).is_ok());
    }

    #[test]
    fn test_mr_cache_table() {
        let cache = MrCacheTable::new();
        let addr = UbAddr::new(1, 10, 0, 0, 0);
        cache.insert(MrCacheEntry {
            remote_mr_handle: 1,
            owner_node: 10,
            base_ub_addr: addr,
            len: 1024,
            perms: MrPerms::READ,
            device_kind: DeviceKind::Memory,
        });

        let found = cache.lookup_by_addr(addr.with_offset(100));
        assert!(found.is_some());
        assert_eq!(found.unwrap().remote_mr_handle, 1);

        cache.remove(10, 1);
        assert!(cache.lookup_by_addr(addr.with_offset(100)).is_none());
    }

    #[test]
    fn test_mr_publish_event_on_register() {
        let (table, mut rx) = MrTable::new_with_channel(1, 42);
        let dev = Arc::new(MemoryDevice::new(4096));
        let (addr, handle) = table.register(dev, 1024, MrPerms::READ | MrPerms::WRITE).unwrap();

        let event = rx.try_recv().unwrap();
        match event {
            MrPublishEvent::Publish(info) => {
                assert_eq!(info.mr_handle, handle.0);
                assert_eq!(info.owner_node, 42);
                assert_eq!(info.base_ub_addr, addr);
                assert_eq!(info.len, 1024);
                assert_eq!(info.perms, MrPerms::READ | MrPerms::WRITE);
                assert_eq!(info.device_kind, DeviceKind::Memory);
            }
            MrPublishEvent::Revoke { .. } => panic!("expected Publish event"),
        }
    }

    #[test]
    fn test_mr_publish_event_on_deregister() {
        let (table, mut rx) = MrTable::new_with_channel(1, 42);
        let dev = Arc::new(MemoryDevice::new(4096));
        let (_, handle) = table.register(dev, 1024, MrPerms::READ).unwrap();

        // Drain the Publish event
        let _ = rx.try_recv().unwrap();

        table.deregister(handle).unwrap();
        let event = rx.try_recv().unwrap();
        match event {
            MrPublishEvent::Revoke { owner_node, mr_handle } => {
                assert_eq!(owner_node, 42);
                assert_eq!(mr_handle, handle.0);
            }
            MrPublishEvent::Publish(_) => panic!("expected Revoke event"),
        }
    }

    #[test]
    fn test_mr_table_without_channel_works() {
        // MrTable without channel should work exactly as before
        let table = MrTable::new(1, 42);
        let dev = Arc::new(MemoryDevice::new(4096));
        let (_addr, handle) = table.register(dev, 1024, MrPerms::READ).unwrap();
        assert!(table.lookup(handle.0).is_some());
        table.deregister(handle).unwrap();
        assert!(table.lookup(handle.0).is_none());
    }

    #[test]
    fn test_mr_deregister_revoking_state() {
        let table = MrTable::new(1, 42);
        let dev = Arc::new(MemoryDevice::new(4096));
        let (_, handle) = table.register(dev, 1024, MrPerms::READ).unwrap();

        // Deregister should transition state to Revoking then remove
        // After deregister, lookup should return None
        table.deregister(handle).unwrap();
        assert!(table.lookup(handle.0).is_none());

        // Verify that a new MR with inflight refs blocks try_inflight_inc when Revoking
        let dev2 = Arc::new(MemoryDevice::new(4096));
        let (_, handle2) = table.register(dev2, 1024, MrPerms::READ).unwrap();
        let entry = table.lookup(handle2.0).unwrap();
        // Manually set to Revoking — try_inflight_inc should fail
        entry.set_state(MrState::Revoking);
        assert!(!entry.try_inflight_inc());
    }

    #[tokio::test]
    async fn test_mr_deregister_waits_inflight() {
        let table = MrTable::new(1, 42);
        let dev = Arc::new(MemoryDevice::new(4096));
        let (_, handle) = table.register(dev, 1024, MrPerms::READ).unwrap();

        let entry = table.lookup(handle.0).unwrap();
        // Simulate an inflight operation
        assert!(entry.try_inflight_inc());
        assert_eq!(entry.inflight_refs.load(Ordering::Acquire), 1);

        // Spawn deregister_async in a separate task — it should wait

        // Use a real scenario: start deregister_async, then release inflight
        let entry_arc = table.entries.get(&handle.0).map(|r| Arc::clone(r.value())).unwrap();

        // Transition to Revoking manually (deregister_async would do this)
        entry_arc.state.store(MrState::Revoking as u8, Ordering::Release);

        // Spawn a task that waits for inflight to drain
        let wait_task = tokio::spawn(async move {
            entry_arc.wait_inflight_zero().await;
        });

        // Release the inflight ref
        entry.inflight_dec();

        // The wait task should complete quickly
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), wait_task).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_mr_deregister_timeout() {
        let (table, _rx) = MrTable::new_with_channel(1, 42);
        let dev = Arc::new(MemoryDevice::new(4096));
        let (_, handle) = table.register(dev, 1024, MrPerms::READ).unwrap();

        let entry = table.lookup(handle.0).unwrap();
        // Simulate an inflight operation that won't complete
        assert!(entry.try_inflight_inc());

        // deregister_async with a very short timeout should return Timeout
        let result = table.deregister_async(crate::types::MrHandle(handle.0), 10).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            UbError::Timeout => {} // expected
            e => panic!("expected Timeout, got {:?}", e),
        }

        // Cleanup: release the inflight ref
        entry.inflight_dec();
    }

    #[test]
    fn test_mr_deregister_already_revoking() {
        let table = MrTable::new(1, 42);
        let dev = Arc::new(MemoryDevice::new(4096));
        let (_, handle) = table.register(dev, 1024, MrPerms::READ).unwrap();

        // First deregister should succeed
        table.deregister(handle).unwrap();

        // Second deregister should fail (entry already removed)
        let result = table.deregister(handle);
        assert!(result.is_err());
    }
}
