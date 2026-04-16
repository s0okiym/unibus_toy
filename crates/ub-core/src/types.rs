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
}
