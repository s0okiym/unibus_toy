use crate::error::UbError;

/// 128-bit UB Address: [PodID:16 | NodeID:16 | DeviceID:16 | Offset:64 | Reserved:16]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct UbAddr(pub u128);

impl UbAddr {
    pub const fn new(pod_id: u16, node_id: u16, device_id: u16, offset: u64, reserved: u16) -> Self {
        let val = ((pod_id as u128) << 112)
            | ((node_id as u128) << 96)
            | ((device_id as u128) << 80)
            | ((offset as u128) << 16)
            | (reserved as u128);
        UbAddr(val)
    }

    pub fn pod_id(&self) -> u16 {
        ((self.0 >> 112) & 0xFFFF) as u16
    }

    pub fn node_id(&self) -> u16 {
        ((self.0 >> 96) & 0xFFFF) as u16
    }

    pub fn device_id(&self) -> u16 {
        ((self.0 >> 80) & 0xFFFF) as u16
    }

    pub fn offset(&self) -> u64 {
        ((self.0 >> 16) & 0xFFFF_FFFF_FFFF_FFFF) as u64
    }

    pub fn reserved(&self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    pub fn with_offset(&self, new_offset: u64) -> Self {
        let mask = !(0xFFFF_FFFF_FFFF_FFFFu128 << 16);
        UbAddr((self.0 & mask) | ((new_offset as u128) << 16))
    }

    /// Text representation: 0x{pod}:{node}:{dev}:{off16}:{res}
    pub fn to_text(&self) -> String {
        format!(
            "0x{:04x}:{:04x}:{:04x}:{:016x}:{:04x}",
            self.pod_id(),
            self.node_id(),
            self.device_id(),
            self.offset(),
            self.reserved()
        )
    }

    /// Parse from text representation: "0x{pod}:{node}:{dev}:{off}:{res}"
    pub fn from_text(s: &str) -> Result<Self, UbError> {
        let s = s.strip_prefix("0x").ok_or_else(|| {
            UbError::Internal(format!("UB address must start with '0x': {s}"))
        })?;

        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 5 {
            return Err(UbError::Internal(format!(
                "UB address must have 5 colon-separated segments: {s}"
            )));
        }

        let pod_id = u16::from_str_radix(parts[0], 16).map_err(|e| {
            UbError::Internal(format!("invalid pod_id '{}': {e}", parts[0]))
        })?;
        let node_id = u16::from_str_radix(parts[1], 16).map_err(|e| {
            UbError::Internal(format!("invalid node_id '{}': {e}", parts[1]))
        })?;
        let device_id = u16::from_str_radix(parts[2], 16).map_err(|e| {
            UbError::Internal(format!("invalid device_id '{}': {e}", parts[2]))
        })?;
        let offset = u64::from_str_radix(parts[3], 16).map_err(|e| {
            UbError::Internal(format!("invalid offset '{}': {e}", parts[3]))
        })?;
        let reserved = u16::from_str_radix(parts[4], 16).map_err(|e| {
            UbError::Internal(format!("invalid reserved '{}': {e}", parts[4]))
        })?;

        Ok(UbAddr::new(pod_id, node_id, device_id, offset, reserved))
    }

    /// Serialize to 16 bytes big-endian.
    pub fn to_bytes(&self) -> [u8; 16] {
        self.0.to_be_bytes()
    }

    /// Deserialize from 16 bytes big-endian.
    pub fn from_bytes(buf: &[u8; 16]) -> Self {
        UbAddr(u128::from_be_bytes(*buf))
    }
}

impl std::fmt::Display for UbAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ub_addr_roundtrip() {
        let addr = UbAddr::new(1, 42, 0, 0xDEADBEEF, 0);
        assert_eq!(addr.pod_id(), 1);
        assert_eq!(addr.node_id(), 42);
        assert_eq!(addr.device_id(), 0);
        assert_eq!(addr.offset(), 0xDEADBEEF);
        assert_eq!(addr.reserved(), 0);
    }

    #[test]
    fn test_ub_addr_text_roundtrip() {
        let addr = UbAddr::new(1, 0x42, 0, 0xDEADBEEF, 0);
        let text = addr.to_text();
        assert_eq!(text, "0x0001:0042:0000:00000000deadbeef:0000");
        let parsed = UbAddr::from_text(&text).unwrap();
        assert_eq!(addr, parsed);
    }

    #[test]
    fn test_ub_addr_bytes_roundtrip() {
        let addr = UbAddr::new(1, 66, 1, 0x1234_5678_9ABC_DEF0, 0);
        let bytes = addr.to_bytes();
        let restored = UbAddr::from_bytes(&bytes);
        assert_eq!(addr, restored);
    }

    #[test]
    fn test_ub_addr_with_offset() {
        let addr = UbAddr::new(1, 42, 0, 0, 0);
        let addr2 = addr.with_offset(1024);
        assert_eq!(addr2.offset(), 1024);
        assert_eq!(addr2.pod_id(), 1);
        assert_eq!(addr2.node_id(), 42);
    }

    #[test]
    fn test_ub_addr_example() {
        let addr = UbAddr::new(1, 66, 0, 0xDEADBEEF, 0);
        let text = addr.to_text();
        assert!(text.starts_with("0x0001:0042:0000:"));
    }

    #[test]
    fn test_ub_addr_parse_errors() {
        assert!(UbAddr::from_text("invalid").is_err());
        assert!(UbAddr::from_text("0x1:2:3").is_err()); // only 3 segments
        assert!(UbAddr::from_text("1:2:3:4:5").is_err()); // missing 0x prefix
        assert!(UbAddr::from_text("0x1:2:3:4:5:6").is_err()); // 6 segments
    }
}
