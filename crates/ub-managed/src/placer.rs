use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;
use ub_core::config::ManagedConfig;
use ub_core::types::{
    AllocHints, DeviceKind, RegionId, UbVa,
};

use crate::registry::DeviceRegistry;
use crate::sub_alloc::SubAllocator;

/// Placement record for a region.
#[derive(Debug, Clone)]
pub struct RegionPlacement {
    pub region_id: RegionId,
    pub home_node_id: u16,
    pub device_id: u16,
    pub mr_handle: u32,
    pub base_offset: u64,
    pub len: u64,
    pub requester_node_id: u16,
}

/// Placer — decides which device should host a new region.
///
/// The placer uses the DeviceRegistry's cost function to select the best device,
/// then records the placement. In a real distributed system, this would send
/// REGION_CREATE to the home node and wait for REGION_CREATE_OK. For the toy
/// implementation, the placer directly manages sub-allocators for each device.
pub struct Placer {
    registry: Arc<DeviceRegistry>,
    regions: RwLock<HashMap<RegionId, RegionPlacement>>,
    next_region_id: AtomicU64,
    /// Sub-allocators per device, keyed by (node_id, device_id).
    sub_allocators: RwLock<HashMap<(u16, u16), Arc<SubAllocator>>>,
}

impl Placer {
    pub fn new(registry: Arc<DeviceRegistry>, _config: ManagedConfig) -> Self {
        Placer {
            registry,
            regions: RwLock::new(HashMap::new()),
            next_region_id: AtomicU64::new(1),
            sub_allocators: RwLock::new(HashMap::new()),
        }
    }

    /// Register a sub-allocator for a device (called when a pool MR is set up).
    pub fn register_sub_allocator(&self, node_id: u16, device_id: u16, allocator: SubAllocator) {
        let mut allocs = self.sub_allocators.write();
        allocs.insert((node_id, device_id), Arc::new(allocator));
    }

    /// Allocate a region. Returns the UbVa and placement info.
    ///
    /// This method:
    /// 1. Selects the best device using the cost function
    /// 2. Sub-allocates within the device's pool MR
    /// 3. Records the placement
    /// 4. Returns the UbVa
    pub fn place(
        &self,
        hints: &AllocHints,
        requester_node_id: u16,
    ) -> Result<(UbVa, RegionPlacement), String> {
        // Select best device
        let pin = match hints.pin {
            Some(DeviceKind::Memory) => Some(DeviceKind::Memory),
            Some(DeviceKind::Npu) => Some(DeviceKind::Npu),
            None => None,
        };

        let best = self.registry.best_device(
            requester_node_id,
            hints.latency_class,
            hints.capacity_class,
            pin,
            hints.size,
        );

        let (home_node_id, device_id) = best.ok_or("no suitable device found for placement")?;

        // Sub-allocate within the device's pool
        let allocs = self.sub_allocators.read();
        let allocator = allocs.get(&(home_node_id, device_id))
            .ok_or_else(|| format!("no sub-allocator for device ({}, {})", home_node_id, device_id))?;
        let base_offset = allocator.alloc(hints.size)
            .ok_or("insufficient space in device pool")?;
        drop(allocs);

        // Assign region ID
        let region_id = self.next_region_id.fetch_add(1, Ordering::AcqRel);
        let region_id = RegionId(region_id);

        // Create UbVa
        let va = UbVa::new(region_id, 0);

        // Record placement
        let placement = RegionPlacement {
            region_id,
            home_node_id,
            device_id,
            mr_handle: 0, // Will be filled in when REGION_CREATE_OK arrives
            base_offset,
            len: hints.size,
            requester_node_id,
        };

        let mut regions = self.regions.write();
        regions.insert(region_id, placement.clone());

        // Update device used_bytes
        self.registry.update_dynamic(home_node_id, device_id, hints.size, 0);

        Ok((va, placement))
    }

    /// Place a region with a pre-determined region ID (for REGION_CREATE_OK callback).
    pub fn place_with_id(
        &self,
        region_id: RegionId,
        hints: &AllocHints,
        requester_node_id: u16,
        mr_handle: u32,
    ) -> Result<(UbVa, RegionPlacement), String> {
        let pin = match hints.pin {
            Some(DeviceKind::Memory) => Some(DeviceKind::Memory),
            Some(DeviceKind::Npu) => Some(DeviceKind::Npu),
            None => None,
        };

        let best = self.registry.best_device(
            requester_node_id,
            hints.latency_class,
            hints.capacity_class,
            pin,
            hints.size,
        );

        let (home_node_id, device_id) = best.ok_or("no suitable device found for placement")?;

        let allocs = self.sub_allocators.read();
        let allocator = allocs.get(&(home_node_id, device_id))
            .ok_or_else(|| format!("no sub-allocator for device ({}, {})", home_node_id, device_id))?;
        let base_offset = allocator.alloc(hints.size)
            .ok_or("insufficient space in device pool")?;
        drop(allocs);

        let va = UbVa::new(region_id, 0);

        let placement = RegionPlacement {
            region_id,
            home_node_id,
            device_id,
            mr_handle,
            base_offset,
            len: hints.size,
            requester_node_id,
        };

        let mut regions = self.regions.write();
        regions.insert(region_id, placement.clone());

        self.registry.update_dynamic(home_node_id, device_id, hints.size, 0);

        Ok((va, placement))
    }

    /// Free a region by ID. Returns the placement info if found.
    pub fn free(&self, region_id: RegionId) -> Option<RegionPlacement> {
        let mut regions = self.regions.write();
        let placement = regions.remove(&region_id)?;

        // Sub-allocator free is a no-op (bump allocator)
        let allocs = self.sub_allocators.read();
        if let Some(allocator) = allocs.get(&(placement.home_node_id, placement.device_id)) {
            allocator.free(placement.base_offset, placement.len);
        }

        // Update device used_bytes (decrement)
        self.registry.update_dynamic(
            placement.home_node_id,
            placement.device_id,
            0, // used_bytes delta not tracked precisely in bump allocator
            0,
        );

        Some(placement)
    }

    /// Look up a region placement by ID.
    pub fn lookup(&self, region_id: RegionId) -> Option<RegionPlacement> {
        self.regions.read().get(&region_id).cloned()
    }

    /// Update the mr_handle for a region (when REGION_CREATE_OK arrives).
    pub fn set_mr_handle(&self, region_id: RegionId, mr_handle: u32) -> bool {
        let mut regions = self.regions.write();
        if let Some(p) = regions.get_mut(&region_id) {
            p.mr_handle = mr_handle;
            true
        } else {
            false
        }
    }

    /// List all region placements.
    pub fn list(&self) -> Vec<RegionPlacement> {
        self.regions.read().values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ub_core::types::{DeviceProfile, LatencyClass};

    fn setup_registry_and_placer() -> (Arc<DeviceRegistry>, Arc<Placer>) {
        let config = ManagedConfig::default();
        let registry = Arc::new(DeviceRegistry::new(config.clone()));
        // Register two devices: memory on node 1, NPU on node 1
        registry.register(DeviceProfile::memory_default(1, 1024 * 1024 * 256));
        registry.register(DeviceProfile::npu_default(1, 1, 1024 * 1024 * 256));

        let placer = Arc::new(Placer::new(Arc::clone(&registry), config));

        // Register sub-allocators
        placer.register_sub_allocator(1, 0, SubAllocator::new(0, 1024 * 1024 * 256));
        placer.register_sub_allocator(1, 1, SubAllocator::new(0, 1024 * 1024 * 256));

        (registry, placer)
    }

    #[test]
    fn test_placement_cost_function() {
        let config = ManagedConfig::default();
        let registry = Arc::new(DeviceRegistry::new(config.clone()));
        // Two devices on the SAME node: Warm (memory) and Hot (NPU)
        registry.register(DeviceProfile::memory_default(1, 1024 * 1024 * 256));
        registry.register(DeviceProfile::npu_default(1, 1, 1024 * 1024 * 256));

        let placer = Placer::new(Arc::clone(&registry), config);
        placer.register_sub_allocator(1, 0, SubAllocator::new(0, 1024 * 1024 * 256));
        placer.register_sub_allocator(1, 1, SubAllocator::new(0, 1024 * 1024 * 256));

        // Critical latency from node 1 should prefer Hot tier (NPU) — same node so latency=0 for both
        let hints = AllocHints {
            latency_class: LatencyClass::Critical,
            ..Default::default()
        };
        let (_, placement) = placer.place(&hints, 1).unwrap();
        assert_eq!(placement.device_id, 1); // NPU device (Hot tier)
        assert_eq!(placement.home_node_id, 1);

        // Normal latency from node 1 should prefer Warm tier (memory) — same node so latency=0 for both
        let hints = AllocHints {
            latency_class: LatencyClass::Normal,
            ..Default::default()
        };
        let (_, placement) = placer.place(&hints, 1).unwrap();
        assert_eq!(placement.device_id, 0); // Memory device (Warm tier)
        assert_eq!(placement.home_node_id, 1);
    }

    #[test]
    fn test_placement_prefers_local_device() {
        let config = ManagedConfig::default();
        let registry = Arc::new(DeviceRegistry::new(config.clone()));
        // Same tier on two nodes
        registry.register(DeviceProfile::memory_default(1, 1024 * 1024 * 256));
        registry.register(DeviceProfile::memory_default(2, 1024 * 1024 * 256));

        let placer = Placer::new(Arc::clone(&registry), config);
        placer.register_sub_allocator(1, 0, SubAllocator::new(0, 1024 * 1024 * 256));
        placer.register_sub_allocator(2, 0, SubAllocator::new(0, 1024 * 1024 * 256));

        let hints = AllocHints {
            latency_class: LatencyClass::Normal,
            ..Default::default()
        };

        // Request from node 1 → prefer node 1
        let (_, placement) = placer.place(&hints, 1).unwrap();
        assert_eq!(placement.home_node_id, 1);
    }

    #[test]
    fn test_placement_respects_pin_hint() {
        let (_registry, placer) = setup_registry_and_placer();

        let hints = AllocHints {
            pin: Some(DeviceKind::Npu),
            latency_class: LatencyClass::Normal,
            ..Default::default()
        };

        let (_, placement) = placer.place(&hints, 1).unwrap();
        assert_eq!(placement.device_id, 1); // NPU device
    }

    #[test]
    fn test_placement_and_free() {
        let (_registry, placer) = setup_registry_and_placer();

        let hints = AllocHints {
            size: 4096,
            latency_class: LatencyClass::Normal,
            ..Default::default()
        };

        let (va, _placement) = placer.place(&hints, 1).unwrap();
        let region_id = va.region_id();

        // Verify placement exists
        let found = placer.lookup(region_id).unwrap();
        assert_eq!(found.region_id, region_id);

        // Free the region
        let freed = placer.free(region_id).unwrap();
        assert_eq!(freed.region_id, region_id);

        // Verify it's gone
        assert!(placer.lookup(region_id).is_none());
    }

    #[test]
    fn test_placement_insufficient_space() {
        let config = ManagedConfig::default();
        let registry = Arc::new(DeviceRegistry::new(config.clone()));
        registry.register(DeviceProfile::memory_default(1, 1024)); // tiny device

        let placer = Placer::new(Arc::clone(&registry), config);
        placer.register_sub_allocator(1, 0, SubAllocator::new(0, 1024));

        let hints = AllocHints {
            size: 2048, // larger than device
            ..Default::default()
        };

        // best_device will return None because free_bytes < size
        let result = placer.place(&hints, 1);
        assert!(result.is_err());
    }

    #[test]
    fn test_placement_no_sub_allocator() {
        let config = ManagedConfig::default();
        let registry = Arc::new(DeviceRegistry::new(config.clone()));
        registry.register(DeviceProfile::memory_default(1, 1024 * 1024 * 256));

        // Placer without sub-allocator registered
        let placer = Placer::new(Arc::clone(&registry), config);

        let hints = AllocHints::default();
        let result = placer.place(&hints, 1);
        assert!(result.is_err());
    }

    #[test]
    fn test_placement_set_mr_handle() {
        let (_registry, placer) = setup_registry_and_placer();

        let hints = AllocHints {
            size: 4096,
            ..Default::default()
        };
        let (va, _) = placer.place(&hints, 1).unwrap();
        let region_id = va.region_id();

        assert!(placer.set_mr_handle(region_id, 42));
        let found = placer.lookup(region_id).unwrap();
        assert_eq!(found.mr_handle, 42);

        assert!(!placer.set_mr_handle(RegionId(9999), 1));
    }
}