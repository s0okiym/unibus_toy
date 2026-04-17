use std::collections::HashMap;

use parking_lot::RwLock;
use ub_core::types::{RegionId, RegionInfo, RegionState};

/// Per-node region table — tracks all regions known to this node.
///
/// For home regions: state = Home, mr_handle/base_offset refer to the local MR.
/// For cached regions: state = Shared(epoch) or Invalid, local_mr_handle refers
/// to the cache pool MR, and the data is at base_offset within that pool MR.
pub struct RegionTable {
    entries: RwLock<HashMap<RegionId, RegionInfo>>,
}

impl RegionTable {
    pub fn new() -> Self {
        RegionTable {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Insert or replace a region entry.
    pub fn insert(&self, info: RegionInfo) {
        let mut entries = self.entries.write();
        entries.insert(info.region_id, info);
    }

    /// Look up a region by ID.
    pub fn lookup(&self, region_id: RegionId) -> Option<RegionInfo> {
        let entries = self.entries.read();
        entries.get(&region_id).cloned()
    }

    /// Remove a region entry.
    pub fn remove(&self, region_id: RegionId) -> Option<RegionInfo> {
        let mut entries = self.entries.write();
        entries.remove(&region_id)
    }

    /// Update the state of a region. Returns false if region not found.
    pub fn set_state(&self, region_id: RegionId, state: RegionState) -> bool {
        let mut entries = self.entries.write();
        if let Some(entry) = entries.get_mut(&region_id) {
            entry.state = state;
            true
        } else {
            false
        }
    }

    /// Update the epoch of a region. Returns false if region not found.
    pub fn set_epoch(&self, region_id: RegionId, epoch: u64) -> bool {
        let mut entries = self.entries.write();
        if let Some(entry) = entries.get_mut(&region_id) {
            entry.epoch = epoch;
            true
        } else {
            false
        }
    }

    /// List all known regions.
    pub fn list(&self) -> Vec<RegionInfo> {
        self.entries.read().values().cloned().collect()
    }

    /// List regions by state.
    pub fn list_by_state(&self, state: RegionState) -> Vec<RegionInfo> {
        self.entries.read().values().filter(|e| e.state == state).cloned().collect()
    }

    /// Count regions by state (for metrics).
    pub fn count_by_state(&self) -> (usize, usize, usize) {
        let entries = self.entries.read();
        let mut home = 0;
        let mut shared = 0;
        let mut invalid = 0;
        for e in entries.values() {
            match e.state {
                RegionState::Home => home += 1,
                RegionState::Shared(_) => shared += 1,
                RegionState::Invalid => invalid += 1,
            }
        }
        (home, shared, invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ub_core::types::RegionState;

    fn make_region(region_id: u64, home_node_id: u16, state: RegionState) -> RegionInfo {
        RegionInfo {
            region_id: RegionId(region_id),
            home_node_id,
            device_id: 0,
            mr_handle: 1,
            base_offset: 0,
            len: 4096,
            epoch: 0,
            state,
            local_mr_handle: None,
        }
    }

    #[test]
    fn test_region_table_insert_lookup_remove() {
        let table = RegionTable::new();
        let info = make_region(1, 1, RegionState::Home);
        table.insert(info.clone());

        let found = table.lookup(RegionId(1)).unwrap();
        assert_eq!(found.region_id, RegionId(1));
        assert_eq!(found.home_node_id, 1);
        assert_eq!(found.state, RegionState::Home);

        let removed = table.remove(RegionId(1)).unwrap();
        assert_eq!(removed.region_id, RegionId(1));
        assert!(table.lookup(RegionId(1)).is_none());
    }

    #[test]
    fn test_region_table_set_state() {
        let table = RegionTable::new();
        table.insert(make_region(1, 1, RegionState::Home));

        assert!(table.set_state(RegionId(1), RegionState::Shared(1)));
        let found = table.lookup(RegionId(1)).unwrap();
        assert_eq!(found.state, RegionState::Shared(1));

        assert!(table.set_state(RegionId(1), RegionState::Invalid));
        let found = table.lookup(RegionId(1)).unwrap();
        assert_eq!(found.state, RegionState::Invalid);

        // Non-existent region
        assert!(!table.set_state(RegionId(999), RegionState::Home));
    }

    #[test]
    fn test_region_table_set_epoch() {
        let table = RegionTable::new();
        table.insert(make_region(1, 1, RegionState::Shared(0)));

        assert!(table.set_epoch(RegionId(1), 5));
        assert_eq!(table.lookup(RegionId(1)).unwrap().epoch, 5);

        assert!(!table.set_epoch(RegionId(999), 1));
    }

    #[test]
    fn test_region_table_list_by_state() {
        let table = RegionTable::new();
        table.insert(make_region(1, 1, RegionState::Home));
        table.insert(make_region(2, 2, RegionState::Shared(1)));
        table.insert(make_region(3, 1, RegionState::Invalid));
        table.insert(make_region(4, 2, RegionState::Home));

        let home = table.list_by_state(RegionState::Home);
        assert_eq!(home.len(), 2);

        let shared = table.list_by_state(RegionState::Shared(1));
        assert_eq!(shared.len(), 1);

        let invalid = table.list_by_state(RegionState::Invalid);
        assert_eq!(invalid.len(), 1);
    }

    #[test]
    fn test_region_table_count_by_state() {
        let table = RegionTable::new();
        table.insert(make_region(1, 1, RegionState::Home));
        table.insert(make_region(2, 2, RegionState::Shared(1)));
        table.insert(make_region(3, 1, RegionState::Invalid));
        table.insert(make_region(4, 2, RegionState::Home));
        table.insert(make_region(5, 2, RegionState::Shared(2)));

        let (home, shared, invalid) = table.count_by_state();
        assert_eq!(home, 2);
        assert_eq!(shared, 2);
        assert_eq!(invalid, 1);
    }

    #[test]
    fn test_region_state_transitions() {
        // Invalid -> Home (first allocation)
        let mut state;
        state = RegionState::Invalid;
        state = RegionState::Home;
        assert_eq!(state, RegionState::Home);

        // Home -> Invalid (free)
        state = RegionState::Invalid;
        assert_eq!(state, RegionState::Invalid);

        // Invalid -> Shared(epoch) (FETCH)
        state = RegionState::Shared(1);
        assert_eq!(state, RegionState::Shared(1));

        // Shared -> Invalid (invalidate)
        state = RegionState::Invalid;
        assert_eq!(state, RegionState::Invalid);
    }
}