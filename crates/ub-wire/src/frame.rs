use bitflags::bitflags;
use ub_core::addr::UbAddr;
use ub_core::types::Verb;

pub const MAGIC: u32 = 0x5542_5459; // "UBTY"
pub const VERSION: u8 = 1;
pub const FRAME_HEADER_SIZE: usize = 32;
pub const DATA_EXT_HEADER_SIZE: usize = 48;
pub const DATA_EXT_HEADER_IMM_SIZE: usize = 56; // 48 + 8 for Imm

/// Data plane frame types (§4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Data = 0x01,
    Ack = 0x02,
    Credit = 0x03,
}

impl FrameType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(FrameType::Data),
            0x02 => Some(FrameType::Ack),
            0x03 => Some(FrameType::Credit),
            _ => None,
        }
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FrameFlags: u16 {
        const ACK_REQ    = 0b0000_0000_0000_0001;
        const FRAG_FIRST = 0b0000_0000_0000_0010;
        const FRAG_MIDDLE= 0b0000_0000_0000_0100;
        const FRAG_LAST  = 0b0000_0000_0000_1000;
        const HAS_IMM    = 0b0000_0000_0001_0000;
        const HAS_SACK   = 0b0000_0000_0010_0000;
        const ERR_RESP   = 0b0000_0000_0100_0000;
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ExtFlags: u8 {
        const HAS_IMM  = 0b0000_0001;
        const FRAG     = 0b0000_0010;
        const ERR_RESP = 0b0000_0100;
    }
}

/// Generic frame header (32 bytes, §4.2).
#[derive(Debug, Clone, PartialEq)]
pub struct FrameHeader {
    pub magic: u32,
    pub version: u8,
    pub frame_type: FrameType,
    pub flags: FrameFlags,
    pub src_node: u16,
    pub dst_node: u16,
    pub reserved: u32,
    pub stream_seq: u64,
    pub payload_len: u32,
    pub header_crc: u32,
}

/// DATA extension header (48 bytes fixed, +8 optional Imm, §4.2).
#[derive(Debug, Clone, PartialEq)]
pub struct DataExtHeader {
    pub verb: Verb,
    pub ext_flags: ExtFlags,
    pub mr_handle: u32,
    pub jetty_src: u32,
    pub jetty_dst: u32,
    pub opaque: u64,
    pub frag_id: u32,
    pub frag_index: u16,
    pub frag_total: u16,
    pub ub_addr: UbAddr,
    pub imm: Option<u64>,
}

/// ACK payload (§5.2).
#[derive(Debug, Clone, PartialEq)]
pub struct AckPayload {
    pub ack_seq: u64,
    pub credit_grant: u32,
    pub reserved: u32,
    pub sack_bitmap: Option<[u8; 32]>, // 256-bit SACK bitmap
}

/// CREDIT payload (§6).
#[derive(Debug, Clone, PartialEq)]
pub struct CreditPayload {
    pub credits: u32,
    pub reserved: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_type_roundtrip() {
        assert_eq!(FrameType::from_u8(0x01), Some(FrameType::Data));
        assert_eq!(FrameType::from_u8(0x02), Some(FrameType::Ack));
        assert_eq!(FrameType::from_u8(0x03), Some(FrameType::Credit));
        assert!(FrameType::from_u8(0x00).is_none());
    }

    #[test]
    fn test_frame_flags() {
        let flags = FrameFlags::ACK_REQ | FrameFlags::HAS_IMM;
        assert!(flags.contains(FrameFlags::ACK_REQ));
        assert!(flags.contains(FrameFlags::HAS_IMM));
        assert!(!flags.contains(FrameFlags::HAS_SACK));
    }

    #[test]
    fn test_ext_flags() {
        let flags = ExtFlags::HAS_IMM | ExtFlags::FRAG;
        assert!(flags.contains(ExtFlags::HAS_IMM));
        assert!(flags.contains(ExtFlags::FRAG));
        assert!(!flags.contains(ExtFlags::ERR_RESP));
    }
}
