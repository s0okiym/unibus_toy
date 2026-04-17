use bitflags::bitflags;
use crate::error::UbStatus;

bitflags! {
    /// MR permission bits (FR-MR-2).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct MrPerms: u8 {
        const READ   = 0b001;
        const WRITE  = 0b010;
        const ATOMIC = 0b100;
    }
}

/// MR handle — local identifier within a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MrHandle(pub u32);

/// Jetty handle — local identifier within a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct JettyHandle(pub u32);

/// Jetty address — globally unique across the SuperPod.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JettyAddr {
    pub node_id: u16,
    pub jetty_id: u32,
}

/// Work Request ID.
pub type WrId = u64;

/// Verb types (matches DATA ext header Verb field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Verb {
    ReadReq = 1,
    ReadResp = 2,
    Write = 3,
    AtomicCas = 4,
    AtomicFaa = 5,
    Send = 6,
    WriteImm = 7,
    AtomicCasResp = 8,
    AtomicFaaResp = 9,
}

impl Verb {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Verb::ReadReq),
            2 => Some(Verb::ReadResp),
            3 => Some(Verb::Write),
            4 => Some(Verb::AtomicCas),
            5 => Some(Verb::AtomicFaa),
            6 => Some(Verb::Send),
            7 => Some(Verb::WriteImm),
            8 => Some(Verb::AtomicCasResp),
            9 => Some(Verb::AtomicFaaResp),
            _ => None,
        }
    }
}

/// Completion Queue Entry.
#[derive(Debug, Clone)]
pub struct Cqe {
    pub wr_id: WrId,
    pub status: UbStatus,
    pub imm: Option<u64>,
    pub byte_len: u32,
    pub jetty_id: u32,
    pub verb: Verb,
}

/// Device kind (FR-DEV-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DeviceKind {
    Memory = 0,
    Npu = 1,
}

impl DeviceKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(DeviceKind::Memory),
            1 => Some(DeviceKind::Npu),
            _ => None,
        }
    }
}

/// Node identifier.
pub type NodeId = u16;

/// Peer state change event emitted by the control plane.
/// Used to notify the data plane / transport layer of membership changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerChangeEvent {
    /// A new peer has joined (Hello/HelloAck received).
    Joined { node_id: u16, epoch: u32 },
    /// A previously Suspect peer has recovered to Active.
    Recovered { node_id: u16 },
    /// A peer has transitioned to Suspect (missed heartbeats).
    Suspect { node_id: u16 },
    /// A peer has been marked Down.
    Down { node_id: u16, epoch: u32 },
}

/// Storage tier for device capability profiles (FR-GVA-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StorageTier {
    Hot = 0,
    Warm = 1,
    Cold = 2,
}

impl StorageTier {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(StorageTier::Hot),
            1 => Some(StorageTier::Warm),
            2 => Some(StorageTier::Cold),
            _ => None,
        }
    }
}

/// Device capability profile for managed layer placement decisions (FR-GVA-2).
#[derive(Debug, Clone)]
pub struct DeviceProfile {
    pub device_key: (u16, u16),  // (node_id, device_id)
    pub kind: DeviceKind,
    pub tier: StorageTier,
    pub capacity_bytes: u64,
    pub peak_read_bw_mbps: u32,
    pub peak_write_bw_mbps: u32,
    pub read_latency_ns_p50: u32,
    pub write_latency_ns_p50: u32,
    pub used_bytes: u64,
    pub recent_rps: u32,
}

impl DeviceProfile {
    /// Default profile for CPU memory devices.
    pub fn memory_default(node_id: u16, capacity_bytes: u64) -> Self {
        DeviceProfile {
            device_key: (node_id, 0),
            kind: DeviceKind::Memory,
            tier: StorageTier::Warm,
            capacity_bytes,
            peak_read_bw_mbps: 10_000,
            peak_write_bw_mbps: 10_000,
            read_latency_ns_p50: 200,
            write_latency_ns_p50: 200,
            used_bytes: 0,
            recent_rps: 0,
        }
    }

    /// Default profile for simulated NPU devices.
    pub fn npu_default(node_id: u16, device_id: u16, capacity_bytes: u64) -> Self {
        DeviceProfile {
            device_key: (node_id, device_id),
            kind: DeviceKind::Npu,
            tier: StorageTier::Hot,
            capacity_bytes,
            peak_read_bw_mbps: 50_000,
            peak_write_bw_mbps: 50_000,
            read_latency_ns_p50: 50,
            write_latency_ns_p50: 50,
            used_bytes: 0,
            recent_rps: 0,
        }
    }

    /// Free bytes = capacity - used.
    pub fn free_bytes(&self) -> u64 {
        self.capacity_bytes.saturating_sub(self.used_bytes)
    }

    /// Free ratio = free / capacity. Returns 0.0 if capacity is 0.
    pub fn free_ratio(&self) -> f64 {
        if self.capacity_bytes == 0 {
            0.0
        } else {
            self.free_bytes() as f64 / self.capacity_bytes as f64
        }
    }
}

/// Latency class hint for placement (§15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LatencyClass {
    Critical = 0,
    Normal = 1,
    Bulk = 2,
}

impl LatencyClass {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(LatencyClass::Critical),
            1 => Some(LatencyClass::Normal),
            2 => Some(LatencyClass::Bulk),
            _ => None,
        }
    }
}

/// Capacity class hint for placement (§15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CapacityClass {
    Small = 0,
    Large = 1,
    Huge = 2,
}

impl CapacityClass {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(CapacityClass::Small),
            1 => Some(CapacityClass::Large),
            2 => Some(CapacityClass::Huge),
            _ => None,
        }
    }
}

/// Region identifier — unique within a SuperPod.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegionId(pub u64);

/// Global virtual address — opaque to applications.
/// Internal encoding: `[region_id:64 | offset:64]` as u128.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UbVa(pub u128);

impl UbVa {
    /// Create a UbVa from region_id and offset within the region.
    pub fn new(region_id: RegionId, offset: u64) -> Self {
        UbVa((region_id.0 as u128) << 64 | (offset as u128))
    }

    /// Extract the region_id from this virtual address.
    pub fn region_id(&self) -> RegionId {
        RegionId((self.0 >> 64) as u64)
    }

    /// Extract the offset within the region from this virtual address.
    pub fn offset(&self) -> u64 {
        self.0 as u64
    }
}

/// Access pattern hint for placement decisions (§15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AccessPattern {
    ReadHeavy = 0,
    WriteHeavy = 1,
    Mixed = 2,
}

impl AccessPattern {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(AccessPattern::ReadHeavy),
            1 => Some(AccessPattern::WriteHeavy),
            2 => Some(AccessPattern::Mixed),
            _ => None,
        }
    }
}

/// Allocation hints passed to ub_alloc (FR-GVA-1).
#[derive(Debug, Clone)]
pub struct AllocHints {
    pub size: u64,
    pub access: AccessPattern,
    pub latency_class: LatencyClass,
    pub capacity_class: CapacityClass,
    pub pin: Option<DeviceKind>,
    pub expected_readers: u32,
}

impl Default for AllocHints {
    fn default() -> Self {
        AllocHints {
            size: 4096,
            access: AccessPattern::Mixed,
            latency_class: LatencyClass::Normal,
            capacity_class: CapacityClass::Small,
            pin: None,
            expected_readers: 0,
        }
    }
}

/// Region state in the local region table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionState {
    /// Region not locally cached / no local knowledge.
    Invalid,
    /// Region cached locally with given epoch (read-only).
    Shared(u64),
    /// This node is the home (authoritative copy).
    Home,
}

/// Information about a region in the local region table.
#[derive(Debug, Clone)]
pub struct RegionInfo {
    pub region_id: RegionId,
    pub home_node_id: u16,
    pub device_id: u16,
    pub mr_handle: u32,
    pub base_offset: u64,
    pub len: u64,
    pub epoch: u64,
    pub state: RegionState,
    /// If this node has a local MR for the cached copy, its handle.
    pub local_mr_handle: Option<u32>,
}

/// MR state for lifecycle management (FR-ADDR-5 / §8.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MrState {
    Active = 0,
    Revoking = 1,
    Released = 2,
}

/// Node state machine states (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NodeState {
    Joining,
    Active,
    Suspect,
    Leaving,
    Down,
}

impl std::fmt::Display for NodeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeState::Joining => write!(f, "Joining"),
            NodeState::Active => write!(f, "Active"),
            NodeState::Suspect => write!(f, "Suspect"),
            NodeState::Leaving => write!(f, "Leaving"),
            NodeState::Down => write!(f, "Down"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mr_perms_bits() {
        assert_eq!(MrPerms::READ.bits(), 0b001);
        assert_eq!(MrPerms::WRITE.bits(), 0b010);
        assert_eq!(MrPerms::ATOMIC.bits(), 0b100);
        let rw = MrPerms::READ | MrPerms::WRITE;
        assert!(rw.contains(MrPerms::READ));
        assert!(rw.contains(MrPerms::WRITE));
        assert!(!rw.contains(MrPerms::ATOMIC));
    }

    #[test]
    fn test_verb_roundtrip() {
        for v in 1u8..=9u8 {
            let verb = Verb::from_u8(v).unwrap();
            assert_eq!(verb as u8, v);
        }
        assert!(Verb::from_u8(0).is_none());
        assert!(Verb::from_u8(10).is_none());
    }

    #[test]
    fn test_device_kind_roundtrip() {
        assert_eq!(DeviceKind::from_u8(0), Some(DeviceKind::Memory));
        assert_eq!(DeviceKind::from_u8(1), Some(DeviceKind::Npu));
        assert_eq!(DeviceKind::from_u8(2), None);
    }

    #[test]
    fn test_error_code_values() {
        use crate::error::*;
        assert_eq!(UB_OK, 0);
        assert_eq!(UB_ERR_ADDR_INVALID, 1);
        assert_eq!(UB_ERR_PERM_DENIED, 2);
        assert_eq!(UB_ERR_ALIGNMENT, 3);
        assert_eq!(UB_ERR_LINK_DOWN, 4);
        assert_eq!(UB_ERR_NO_RESOURCES, 5);
        assert_eq!(UB_ERR_TIMEOUT, 6);
        assert_eq!(UB_ERR_PAYLOAD_TOO_LARGE, 7);
        assert_eq!(UB_ERR_FLUSHED, 8);
        assert_eq!(UB_ERR_INTERNAL, 9);
    }

    #[test]
    fn test_node_state_display() {
        assert_eq!(format!("{}", NodeState::Active), "Active");
        assert_eq!(format!("{}", NodeState::Suspect), "Suspect");
    }

    #[test]
    fn test_peer_change_event_equality() {
        let e1 = PeerChangeEvent::Joined { node_id: 2, epoch: 100 };
        let e2 = PeerChangeEvent::Joined { node_id: 2, epoch: 100 };
        let e3 = PeerChangeEvent::Joined { node_id: 2, epoch: 99 };
        assert_eq!(e1, e2);
        assert_ne!(e1, e3);

        let s1 = PeerChangeEvent::Suspect { node_id: 2 };
        let s2 = PeerChangeEvent::Suspect { node_id: 3 };
        assert_ne!(s1, s2);

        let r1 = PeerChangeEvent::Recovered { node_id: 2 };
        assert_ne!(s1, r1);

        let d1 = PeerChangeEvent::Down { node_id: 2, epoch: 100 };
        let d2 = PeerChangeEvent::Down { node_id: 2, epoch: 101 };
        assert_ne!(d1, d2);
    }

    #[test]
    fn test_storage_tier_roundtrip() {
        assert_eq!(StorageTier::from_u8(0), Some(StorageTier::Hot));
        assert_eq!(StorageTier::from_u8(1), Some(StorageTier::Warm));
        assert_eq!(StorageTier::from_u8(2), Some(StorageTier::Cold));
        assert_eq!(StorageTier::from_u8(3), None);
        assert_eq!(StorageTier::Hot as u8, 0);
        assert_eq!(StorageTier::Warm as u8, 1);
        assert_eq!(StorageTier::Cold as u8, 2);
    }

    #[test]
    fn test_device_profile_memory_default() {
        let p = DeviceProfile::memory_default(1, 1024 * 1024 * 256);
        assert_eq!(p.device_key, (1, 0));
        assert_eq!(p.kind, DeviceKind::Memory);
        assert_eq!(p.tier, StorageTier::Warm);
        assert_eq!(p.peak_read_bw_mbps, 10_000);
        assert_eq!(p.read_latency_ns_p50, 200);
        assert_eq!(p.free_ratio(), 1.0);
    }

    #[test]
    fn test_device_profile_npu_default() {
        let p = DeviceProfile::npu_default(1, 1, 1024 * 1024 * 256);
        assert_eq!(p.device_key, (1, 1));
        assert_eq!(p.kind, DeviceKind::Npu);
        assert_eq!(p.tier, StorageTier::Hot);
        assert_eq!(p.peak_read_bw_mbps, 50_000);
        assert_eq!(p.read_latency_ns_p50, 50);
    }

    #[test]
    fn test_device_profile_free_ratio() {
        let mut p = DeviceProfile::memory_default(1, 1000);
        assert_eq!(p.free_bytes(), 1000);
        assert!((p.free_ratio() - 1.0).abs() < f64::EPSILON);
        p.used_bytes = 300;
        assert_eq!(p.free_bytes(), 700);
        assert!((p.free_ratio() - 0.7).abs() < f64::EPSILON);
        p.used_bytes = 1000;
        assert_eq!(p.free_bytes(), 0);
        assert!((p.free_ratio() - 0.0).abs() < f64::EPSILON);
        // Overflow: used > capacity should not panic
        p.used_bytes = 2000;
        assert_eq!(p.free_bytes(), 0);
    }

    #[test]
    fn test_ub_va_encoding() {
        let va = UbVa::new(RegionId(42), 1024);
        assert_eq!(va.region_id(), RegionId(42));
        assert_eq!(va.offset(), 1024);

        // Roundtrip through u128
        let raw = va.0;
        let va2 = UbVa(raw);
        assert_eq!(va2.region_id(), RegionId(42));
        assert_eq!(va2.offset(), 1024);

        // Zero offset
        let va3 = UbVa::new(RegionId(1), 0);
        assert_eq!(va3.region_id(), RegionId(1));
        assert_eq!(va3.offset(), 0);

        // Max values
        let va4 = UbVa::new(RegionId(u64::MAX), u64::MAX);
        assert_eq!(va4.region_id(), RegionId(u64::MAX));
        assert_eq!(va4.offset(), u64::MAX);
    }

    #[test]
    fn test_alloc_hints_default() {
        let hints = AllocHints::default();
        assert_eq!(hints.size, 4096);
        assert_eq!(hints.access, AccessPattern::Mixed);
        assert_eq!(hints.latency_class, LatencyClass::Normal);
        assert_eq!(hints.capacity_class, CapacityClass::Small);
        assert!(hints.pin.is_none());
        assert_eq!(hints.expected_readers, 0);
    }

    #[test]
    fn test_latency_class_roundtrip() {
        assert_eq!(LatencyClass::from_u8(0), Some(LatencyClass::Critical));
        assert_eq!(LatencyClass::from_u8(1), Some(LatencyClass::Normal));
        assert_eq!(LatencyClass::from_u8(2), Some(LatencyClass::Bulk));
        assert_eq!(LatencyClass::from_u8(3), None);
    }

    #[test]
    fn test_capacity_class_roundtrip() {
        assert_eq!(CapacityClass::from_u8(0), Some(CapacityClass::Small));
        assert_eq!(CapacityClass::from_u8(1), Some(CapacityClass::Large));
        assert_eq!(CapacityClass::from_u8(2), Some(CapacityClass::Huge));
        assert_eq!(CapacityClass::from_u8(3), None);
    }

    #[test]
    fn test_access_pattern_roundtrip() {
        assert_eq!(AccessPattern::from_u8(0), Some(AccessPattern::ReadHeavy));
        assert_eq!(AccessPattern::from_u8(1), Some(AccessPattern::WriteHeavy));
        assert_eq!(AccessPattern::from_u8(2), Some(AccessPattern::Mixed));
        assert_eq!(AccessPattern::from_u8(3), None);
    }
}
