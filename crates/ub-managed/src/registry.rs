use parking_lot::RwLock;
use ub_core::config::ManagedConfig;
use ub_core::types::{CapacityClass, DeviceKind, DeviceProfile, LatencyClass, StorageTier};

/// Device capability registry — stores device profiles from all nodes (FR-GVA-2).
///
/// The placer queries this registry to make placement decisions.
/// Profiles are populated via DEVICE_PROFILE_PUBLISH control messages.
pub struct DeviceRegistry {
    profiles: RwLock<Vec<DeviceProfile>>,
    config: ManagedConfig,
}

impl DeviceRegistry {
    pub fn new(config: ManagedConfig) -> Self {
        DeviceRegistry {
            profiles: RwLock::new(Vec::new()),
            config,
        }
    }

    /// Register or update a device profile. If a profile with the same
    /// device_key already exists, it is replaced.
    pub fn register(&self, profile: DeviceProfile) {
        let mut profiles = self.profiles.write();
        if let Some(existing) = profiles.iter_mut().find(|p| p.device_key == profile.device_key) {
            *existing = profile;
        } else {
            profiles.push(profile);
        }
    }

    /// Register multiple profiles at once.
    pub fn register_all(&self, new_profiles: Vec<DeviceProfile>) {
        for p in new_profiles {
            self.register(p);
        }
    }

    /// Look up a profile by device_key (node_id, device_id).
    pub fn get(&self, node_id: u16, device_id: u16) -> Option<DeviceProfile> {
        let profiles = self.profiles.read();
        profiles.iter().find(|p| p.device_key == (node_id, device_id)).cloned()
    }

    /// Return all registered profiles.
    pub fn list(&self) -> Vec<DeviceProfile> {
        self.profiles.read().clone()
    }

    /// Update dynamic fields (used_bytes, recent_rps) for a device.
    pub fn update_dynamic(&self, node_id: u16, device_id: u16, used_bytes: u64, recent_rps: u32) {
        let mut profiles = self.profiles.write();
        if let Some(p) = profiles.iter_mut().find(|p| p.device_key == (node_id, device_id)) {
            p.used_bytes = used_bytes;
            p.recent_rps = recent_rps;
        }
    }

    /// Select the best device for a placement request using the cost function (§16.2).
    ///
    /// Cost function:
    ///   score(d) = w_lat * latency_estimate(d, requester_node, access)
    ///            + w_cap * (1 - free_ratio(d))
    ///            + w_tier * tier_mismatch(latency_class, d.tier)
    ///            + w_load * d.recent_rps / max(d.peak_rps, 1)
    ///
    /// Returns the device_key of the device with the lowest score, or None if
    /// no suitable device is found.
    pub fn best_device(
        &self,
        requester_node: u16,
        latency_class: LatencyClass,
        _capacity_class: CapacityClass,
        pin: Option<DeviceKind>,
        size: u64,
    ) -> Option<(u16, u16)> {
        let profiles = self.profiles.read();
        let weights = &self.config.cost_weights;

        let mut best: Option<(u16, u16, f64)> = None;

        for p in &*profiles {
            // Filter: must have enough free space
            if p.free_bytes() < size {
                continue;
            }
            // Filter: if pin is set, device kind must match
            if let Some(ref kind) = pin {
                if p.kind != *kind {
                    continue;
                }
            }

            let score = weights.latency * latency_estimate(p, requester_node)
                + weights.capacity * (1.0 - p.free_ratio())
                + weights.tier_match * tier_mismatch(latency_class, p.tier)
                + weights.load * (p.recent_rps as f64 / (p.peak_read_bw_mbps as f64).max(1.0));

            match best {
                None => best = Some((p.device_key.0, p.device_key.1, score)),
                Some((_, _, best_score)) if score < best_score => {
                    best = Some((p.device_key.0, p.device_key.1, score));
                }
                _ => {}
            }
        }

        best.map(|(n, d, _)| (n, d))
    }

    /// Remove all profiles for a given node (called when node goes down).
    pub fn remove_node(&self, node_id: u16) {
        let mut profiles = self.profiles.write();
        profiles.retain(|p| p.device_key.0 != node_id);
    }
}

/// Estimate the latency from a device to the requester node.
/// Simplified: if the device is on the same node, latency is ~0 (local);
/// otherwise, use the device's read_latency_ns_p50 as a base.
fn latency_estimate(profile: &DeviceProfile, requester_node: u16) -> f64 {
    if profile.device_key.0 == requester_node {
        0.0 // local access
    } else {
        // Remote: device read latency + network estimate (~5000ns baseline)
        (profile.read_latency_ns_p50 as f64 + 5000.0) / 1000.0 // normalize to ~ms scale
    }
}

/// Compute tier mismatch penalty.
/// Hot tier best for Critical, Warm for Normal, Cold for Bulk.
fn tier_mismatch(latency_class: LatencyClass, tier: StorageTier) -> f64 {
    match (latency_class, tier) {
        // Perfect matches
        (LatencyClass::Critical, StorageTier::Hot) => 0.0,
        (LatencyClass::Normal, StorageTier::Warm) => 0.0,
        (LatencyClass::Bulk, StorageTier::Cold) => 0.0,
        // Acceptable
        (LatencyClass::Critical, StorageTier::Warm) => 0.3,
        (LatencyClass::Normal, StorageTier::Hot) => 0.1,
        (LatencyClass::Normal, StorageTier::Cold) => 0.3,
        (LatencyClass::Bulk, StorageTier::Warm) => 0.1,
        // Poor
        (LatencyClass::Critical, StorageTier::Cold) => 1.0,
        (LatencyClass::Bulk, StorageTier::Hot) => 0.5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ub_core::config::ManagedConfig;

    fn test_config() -> ManagedConfig {
        ManagedConfig::default()
    }

    #[test]
    fn test_device_registry_register_and_list() {
        let registry = DeviceRegistry::new(test_config());
        let p1 = DeviceProfile::memory_default(1, 1024);
        let p2 = DeviceProfile::npu_default(1, 1, 2048);
        registry.register(p1);
        registry.register(p2);

        let list = registry.list();
        assert_eq!(list.len(), 2);

        let found = registry.get(1, 0).unwrap();
        assert_eq!(found.kind, DeviceKind::Memory);

        let found_npu = registry.get(1, 1).unwrap();
        assert_eq!(found_npu.kind, DeviceKind::Npu);
    }

    #[test]
    fn test_device_registry_update_dynamic() {
        let registry = DeviceRegistry::new(test_config());
        registry.register(DeviceProfile::memory_default(1, 1000));
        assert_eq!(registry.get(1, 0).unwrap().used_bytes, 0);

        registry.update_dynamic(1, 0, 500, 100);
        let p = registry.get(1, 0).unwrap();
        assert_eq!(p.used_bytes, 500);
        assert_eq!(p.recent_rps, 100);
    }

    #[test]
    fn test_device_registry_register_replaces_existing() {
        let registry = DeviceRegistry::new(test_config());
        registry.register(DeviceProfile::memory_default(1, 1000));
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.get(1, 0).unwrap().capacity_bytes, 1000);

        // Register again with same key — should replace
        registry.register(DeviceProfile::memory_default(1, 2000));
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.get(1, 0).unwrap().capacity_bytes, 2000);
    }

    #[test]
    fn test_device_registry_remove_node() {
        let registry = DeviceRegistry::new(test_config());
        registry.register(DeviceProfile::memory_default(1, 1000));
        registry.register(DeviceProfile::npu_default(1, 1, 2000));
        registry.register(DeviceProfile::memory_default(2, 3000));
        assert_eq!(registry.list().len(), 3);

        registry.remove_node(1);
        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].device_key.0, 2);
    }

    #[test]
    fn test_device_registry_best_device_selection() {
        let registry = DeviceRegistry::new(test_config());
        // Two devices on node 1: memory (Warm) and NPU (Hot)
        registry.register(DeviceProfile::memory_default(1, 1024 * 1024 * 256));
        registry.register(DeviceProfile::npu_default(1, 1, 1024 * 1024 * 256));
        // One device on node 2: memory (Warm)
        registry.register(DeviceProfile::memory_default(2, 1024 * 1024 * 256));

        // Critical latency class should prefer Hot tier (NPU)
        let best = registry.best_device(1, LatencyClass::Critical, CapacityClass::Small, None, 1024);
        assert!(best.is_some());
        let (node, dev) = best.unwrap();
        assert_eq!(node, 1);
        assert_eq!(dev, 1); // NPU device

        // Normal latency class should prefer Warm tier (Memory) on local node
        let best = registry.best_device(1, LatencyClass::Normal, CapacityClass::Small, None, 1024);
        assert!(best.is_some());
        let (node, dev) = best.unwrap();
        assert_eq!(node, 1);
        assert_eq!(dev, 0); // Memory device on local node

        // Pin to NPU — should only return NPU devices
        let best = registry.best_device(2, LatencyClass::Normal, CapacityClass::Small, Some(DeviceKind::Npu), 1024);
        assert!(best.is_some());
        let (_node, dev) = best.unwrap();
        assert_eq!(dev, 1); // NPU device (on remote node 1)

        // Insufficient capacity — should return None
        let best = registry.best_device(1, LatencyClass::Normal, CapacityClass::Small, None, 1024 * 1024 * 1024);
        assert!(best.is_none());
    }

    #[test]
    fn test_device_registry_best_device_prefers_local() {
        let registry = DeviceRegistry::new(test_config());
        // Same tier on different nodes
        registry.register(DeviceProfile::memory_default(1, 1024 * 1024 * 256));
        registry.register(DeviceProfile::memory_default(2, 1024 * 1024 * 256));

        // Request from node 1 should prefer node 1 (local)
        let best = registry.best_device(1, LatencyClass::Normal, CapacityClass::Small, None, 1024);
        assert_eq!(best.unwrap().0, 1);

        // Request from node 2 should prefer node 2 (local)
        let best = registry.best_device(2, LatencyClass::Normal, CapacityClass::Small, None, 1024);
        assert_eq!(best.unwrap().0, 2);
    }

    #[test]
    fn test_tier_mismatch_values() {
        // Perfect matches
        assert_eq!(tier_mismatch(LatencyClass::Critical, StorageTier::Hot), 0.0);
        assert_eq!(tier_mismatch(LatencyClass::Normal, StorageTier::Warm), 0.0);
        assert_eq!(tier_mismatch(LatencyClass::Bulk, StorageTier::Cold), 0.0);
        // Worst mismatch
        assert_eq!(tier_mismatch(LatencyClass::Critical, StorageTier::Cold), 1.0);
        // Acceptable
        assert!(tier_mismatch(LatencyClass::Critical, StorageTier::Warm) > 0.0);
        assert!(tier_mismatch(LatencyClass::Critical, StorageTier::Warm) < 1.0);
    }
}