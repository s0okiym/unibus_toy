use byteorder::{BigEndian, ReadBytesExt};
use bytes::{BufMut, BytesMut};
use std::io::{Cursor, Read};

use ub_core::addr::UbAddr;
use ub_core::error::UbError;
use ub_core::types::{AccessPattern, CapacityClass, DeviceKind, LatencyClass, MrPerms, StorageTier};

/// Control plane message type enum (§7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CtrlMsgType {
    Hello = 0x01,
    HelloAck = 0x02,
    MemberUp = 0x03,
    MemberDown = 0x04,
    MemberSnapshot = 0x05,
    MrPublish = 0x06,
    MrRevoke = 0x07,
    Heartbeat = 0x08,
    HeartbeatAck = 0x09,
    Join = 0x10,
    DeviceProfilePublish = 0x11,
    AllocReq = 0x12,
    AllocResp = 0x13,
    RegionCreate = 0x14,
    RegionCreateOk = 0x15,
    RegionDelete = 0x16,
    Fetch = 0x17,
    FetchResp = 0x18,
    WriteLockReq = 0x19,
    WriteLockGranted = 0x1A,
    WriteUnlock = 0x1B,
    Invalidate = 0x1C,
    InvalidateAck = 0x1D,
}

impl CtrlMsgType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(CtrlMsgType::Hello),
            0x02 => Some(CtrlMsgType::HelloAck),
            0x03 => Some(CtrlMsgType::MemberUp),
            0x04 => Some(CtrlMsgType::MemberDown),
            0x05 => Some(CtrlMsgType::MemberSnapshot),
            0x06 => Some(CtrlMsgType::MrPublish),
            0x07 => Some(CtrlMsgType::MrRevoke),
            0x08 => Some(CtrlMsgType::Heartbeat),
            0x09 => Some(CtrlMsgType::HeartbeatAck),
            0x10 => Some(CtrlMsgType::Join),
            0x11 => Some(CtrlMsgType::DeviceProfilePublish),
            0x12 => Some(CtrlMsgType::AllocReq),
            0x13 => Some(CtrlMsgType::AllocResp),
            0x14 => Some(CtrlMsgType::RegionCreate),
            0x15 => Some(CtrlMsgType::RegionCreateOk),
            0x16 => Some(CtrlMsgType::RegionDelete),
            0x17 => Some(CtrlMsgType::Fetch),
            0x18 => Some(CtrlMsgType::FetchResp),
            0x19 => Some(CtrlMsgType::WriteLockReq),
            0x1A => Some(CtrlMsgType::WriteLockGranted),
            0x1B => Some(CtrlMsgType::WriteUnlock),
            0x1C => Some(CtrlMsgType::Invalidate),
            0x1D => Some(CtrlMsgType::InvalidateAck),
            _ => None,
        }
    }
}

/// Control plane framing: Length(4B) + MsgType(1B) + Payload(NB).
pub fn encode_ctrl_message(msg_type: CtrlMsgType, payload: &[u8]) -> BytesMut {
    let total = 4 + 1 + payload.len();
    let mut buf = BytesMut::with_capacity(total);
    buf.put_u32(payload.len() as u32);
    buf.put_u8(msg_type as u8);
    buf.put_slice(payload);
    buf
}

/// Decode control plane framing. Returns (MsgType, payload).
pub fn decode_ctrl_message(buf: &[u8]) -> Result<(CtrlMsgType, Vec<u8>), UbError> {
    if buf.len() < 5 {
        return Err(UbError::Internal("control message too short".into()));
    }
    let mut cur = Cursor::new(buf);
    let len = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))? as usize;
    let msg_type_u8 = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
    let msg_type = CtrlMsgType::from_u8(msg_type_u8)
        .ok_or_else(|| UbError::Internal(format!("unknown ctrl msg type: {msg_type_u8}")))?;

    if buf.len() < 5 + len {
        return Err(UbError::Internal("control message truncated".into()));
    }
    let payload = buf[5..5 + len].to_vec();
    Ok((msg_type, payload))
}

/// HELLO message payload.
#[derive(Debug, Clone)]
pub struct HelloPayload {
    pub node_id: u16,
    pub version: u16,
    pub local_epoch: u32,
    pub initial_credits: u32,
    pub data_addr: String,
}

impl HelloPayload {
    pub fn encode(&self) -> Vec<u8> {
        let data_addr_bytes = self.data_addr.as_bytes();
        let mut buf = Vec::with_capacity(16 + data_addr_bytes.len());
        buf.put_u16(self.node_id);
        buf.put_u16(self.version);
        buf.put_u32(self.local_epoch);
        buf.put_u32(self.initial_credits);
        buf.put_u32(data_addr_bytes.len() as u32);
        buf.put_slice(data_addr_bytes);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let node_id = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let version = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let local_epoch = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let initial_credits = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let addr_len = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))? as usize;
        let mut addr_buf = vec![0u8; addr_len];
        cur.read_exact(&mut addr_buf).map_err(|e| UbError::Internal(e.to_string()))?;
        let data_addr = String::from_utf8(addr_buf)
            .map_err(|e| UbError::Internal(format!("invalid data_addr: {e}")))?;

        Ok(HelloPayload {
            node_id,
            version,
            local_epoch,
            initial_credits,
            data_addr,
        })
    }
}

/// HEARTBEAT message payload.
#[derive(Debug, Clone)]
pub struct HeartbeatPayload {
    pub node_id: u16,
    pub timestamp: u64,
}

impl HeartbeatPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(10);
        buf.put_u16(self.node_id);
        buf.put_u64(self.timestamp);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let node_id = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let timestamp = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(HeartbeatPayload { node_id, timestamp })
    }
}

/// MR_PUBLISH message payload.
#[derive(Debug, Clone)]
pub struct MrPublishPayload {
    pub owner_node: u16,
    pub mr_handle: u32,
    pub base_ub_addr: UbAddr,
    pub len: u64,
    pub perms: MrPerms,
    pub device_kind: DeviceKind,
}

impl MrPublishPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(32);
        buf.put_u16(self.owner_node);
        buf.put_u32(self.mr_handle);
        buf.put_slice(&self.base_ub_addr.to_bytes());
        buf.put_u64(self.len);
        buf.put_u8(self.perms.bits());
        buf.put_u8(self.device_kind as u8);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let owner_node = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let mr_handle = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let mut addr_bytes = [0u8; 16];
        cur.read_exact(&mut addr_bytes).map_err(|e| UbError::Internal(e.to_string()))?;
        let base_ub_addr = UbAddr::from_bytes(&addr_bytes);
        let len = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let perms_bits = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
        let perms = MrPerms::from_bits_truncate(perms_bits);
        let device_kind_u8 = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
        let device_kind = DeviceKind::from_u8(device_kind_u8)
            .ok_or_else(|| UbError::Internal(format!("invalid device_kind: {device_kind_u8}")))?;

        Ok(MrPublishPayload {
            owner_node,
            mr_handle,
            base_ub_addr,
            len,
            perms,
            device_kind,
        })
    }
}

/// MR_REVOKE message payload.
#[derive(Debug, Clone)]
pub struct MrRevokePayload {
    pub owner_node: u16,
    pub mr_handle: u32,
}

impl MrRevokePayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(6);
        buf.put_u16(self.owner_node);
        buf.put_u32(self.mr_handle);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let owner_node = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let mr_handle = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(MrRevokePayload { owner_node, mr_handle })
    }
}

/// MEMBER_DOWN message payload.
#[derive(Debug, Clone)]
pub struct MemberDownPayload {
    pub node_id: u16,
    pub reason: u8, // 0=graceful, 1=timeout
}

impl MemberDownPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(3);
        buf.put_u16(self.node_id);
        buf.put_u8(self.reason);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let node_id = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let reason = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(MemberDownPayload { node_id, reason })
    }
}

/// JOIN message payload (FR-CTRL-1 seed mode).
#[derive(Debug, Clone)]
pub struct JoinPayload {
    pub node_id: u16,
    pub epoch: u32,
    pub data_addr: String,
    pub control_addr: String,
    pub initial_credits: u32,
}

impl JoinPayload {
    pub fn encode(&self) -> Vec<u8> {
        let data_bytes = self.data_addr.as_bytes();
        let ctrl_bytes = self.control_addr.as_bytes();
        let mut buf = Vec::with_capacity(14 + data_bytes.len() + ctrl_bytes.len());
        buf.put_u16(self.node_id);
        buf.put_u32(self.epoch);
        buf.put_u32(data_bytes.len() as u32);
        buf.put_slice(data_bytes);
        buf.put_u32(ctrl_bytes.len() as u32);
        buf.put_slice(ctrl_bytes);
        buf.put_u32(self.initial_credits);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let node_id = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let epoch = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let data_len = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))? as usize;
        let mut data_buf = vec![0u8; data_len];
        cur.read_exact(&mut data_buf).map_err(|e| UbError::Internal(e.to_string()))?;
        let data_addr = String::from_utf8(data_buf)
            .map_err(|e| UbError::Internal(format!("invalid data_addr: {e}")))?;
        let ctrl_len = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))? as usize;
        let mut ctrl_buf = vec![0u8; ctrl_len];
        cur.read_exact(&mut ctrl_buf).map_err(|e| UbError::Internal(e.to_string()))?;
        let control_addr = String::from_utf8(ctrl_buf)
            .map_err(|e| UbError::Internal(format!("invalid control_addr: {e}")))?;
        let initial_credits = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(JoinPayload {
            node_id,
            epoch,
            data_addr,
            control_addr,
            initial_credits,
        })
    }
}

/// Single node info entry in a MemberSnapshot (FR-CTRL-1).
#[derive(Debug, Clone)]
pub struct SnapshotNodeInfo {
    pub node_id: u16,
    pub state: u8,
    pub data_addr: String,
    pub control_addr: String,
    pub epoch: u32,
}

/// MEMBER_SNAPSHOT message payload (FR-CTRL-1 seed mode).
#[derive(Debug, Clone)]
pub struct MemberSnapshotPayload {
    pub nodes: Vec<SnapshotNodeInfo>,
}

impl MemberSnapshotPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.put_u32(self.nodes.len() as u32);
        for node in &self.nodes {
            buf.put_u16(node.node_id);
            buf.put_u8(node.state);
            buf.put_u32(node.epoch);
            buf.put_u32(node.data_addr.len() as u32);
            buf.put_slice(node.data_addr.as_bytes());
            buf.put_u32(node.control_addr.len() as u32);
            buf.put_slice(node.control_addr.as_bytes());
        }
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let count = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))? as usize;
        let mut nodes = Vec::with_capacity(count);
        for _ in 0..count {
            let node_id = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
            let state = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
            let epoch = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
            let data_len = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))? as usize;
            let mut data_buf = vec![0u8; data_len];
            cur.read_exact(&mut data_buf).map_err(|e| UbError::Internal(e.to_string()))?;
            let data_addr = String::from_utf8(data_buf)
                .map_err(|e| UbError::Internal(format!("invalid data_addr: {e}")))?;
            let ctrl_len = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))? as usize;
            let mut ctrl_buf = vec![0u8; ctrl_len];
            cur.read_exact(&mut ctrl_buf).map_err(|e| UbError::Internal(e.to_string()))?;
            let control_addr = String::from_utf8(ctrl_buf)
                .map_err(|e| UbError::Internal(format!("invalid control_addr: {e}")))?;
            nodes.push(SnapshotNodeInfo {
                node_id,
                state,
                data_addr,
                control_addr,
                epoch,
            });
        }
        Ok(MemberSnapshotPayload { nodes })
    }
}

/// Single device profile entry in a DEVICE_PROFILE_PUBLISH message (FR-GVA-2).
#[derive(Debug, Clone)]
pub struct DeviceProfileEntry {
    pub node_id: u16,
    pub device_id: u16,
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

impl DeviceProfileEntry {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(40);
        buf.put_u16(self.node_id);
        buf.put_u16(self.device_id);
        buf.put_u8(self.kind as u8);
        buf.put_u8(self.tier as u8);
        buf.put_u64(self.capacity_bytes);
        buf.put_u32(self.peak_read_bw_mbps);
        buf.put_u32(self.peak_write_bw_mbps);
        buf.put_u32(self.read_latency_ns_p50);
        buf.put_u32(self.write_latency_ns_p50);
        buf.put_u64(self.used_bytes);
        buf.put_u32(self.recent_rps);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let node_id = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let device_id = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let kind_u8 = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
        let kind = DeviceKind::from_u8(kind_u8)
            .ok_or_else(|| UbError::Internal(format!("invalid device_kind: {kind_u8}")))?;
        let tier_u8 = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
        let tier = StorageTier::from_u8(tier_u8)
            .ok_or_else(|| UbError::Internal(format!("invalid storage_tier: {tier_u8}")))?;
        let capacity_bytes = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let peak_read_bw_mbps = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let peak_write_bw_mbps = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let read_latency_ns_p50 = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let write_latency_ns_p50 = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let used_bytes = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let recent_rps = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(DeviceProfileEntry {
            node_id,
            device_id,
            kind,
            tier,
            capacity_bytes,
            peak_read_bw_mbps,
            peak_write_bw_mbps,
            read_latency_ns_p50,
            write_latency_ns_p50,
            used_bytes,
            recent_rps,
        })
    }
}

/// DEVICE_PROFILE_PUBLISH message payload (FR-GVA-2).
#[derive(Debug, Clone)]
pub struct DeviceProfilePublishPayload {
    pub entries: Vec<DeviceProfileEntry>,
}

impl DeviceProfilePublishPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.put_u32(self.entries.len() as u32);
        for entry in &self.entries {
            buf.put_u32(entry.encode().len() as u32);
            buf.put_slice(&entry.encode());
        }
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let count = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))? as usize;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let entry_len = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))? as usize;
            let mut entry_buf = vec![0u8; entry_len];
            cur.read_exact(&mut entry_buf).map_err(|e| UbError::Internal(e.to_string()))?;
            entries.push(DeviceProfileEntry::decode(&entry_buf)?);
        }
        Ok(DeviceProfilePublishPayload { entries })
    }
}

/// ALLOC_REQ message payload (FR-GVA-1): requester → placer.
#[derive(Debug, Clone)]
pub struct AllocReqPayload {
    pub region_id: u64,
    pub size: u64,
    pub access: AccessPattern,
    pub latency_class: LatencyClass,
    pub capacity_class: CapacityClass,
    pub pin_kind: u8, // 0=None, 1=Memory, 2=Npu
    pub expected_readers: u32,
    pub requester_node_id: u16,
}

impl AllocReqPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(32);
        buf.put_u64(self.region_id);
        buf.put_u64(self.size);
        buf.put_u8(self.access as u8);
        buf.put_u8(self.latency_class as u8);
        buf.put_u8(self.capacity_class as u8);
        buf.put_u8(self.pin_kind);
        buf.put_u32(self.expected_readers);
        buf.put_u16(self.requester_node_id);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let region_id = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let size = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let access_u8 = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
        let access = AccessPattern::from_u8(access_u8)
            .ok_or_else(|| UbError::Internal(format!("invalid access_pattern: {access_u8}")))?;
        let lc_u8 = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
        let latency_class = LatencyClass::from_u8(lc_u8)
            .ok_or_else(|| UbError::Internal(format!("invalid latency_class: {lc_u8}")))?;
        let cc_u8 = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
        let capacity_class = CapacityClass::from_u8(cc_u8)
            .ok_or_else(|| UbError::Internal(format!("invalid capacity_class: {cc_u8}")))?;
        let pin_kind = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
        let expected_readers = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let requester_node_id = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(AllocReqPayload {
            region_id, size, access, latency_class, capacity_class, pin_kind,
            expected_readers, requester_node_id,
        })
    }
}

/// ALLOC_RESP message payload (FR-GVA-1): placer → requester.
#[derive(Debug, Clone)]
pub struct AllocRespPayload {
    pub region_id: u64,
    /// UbVa as u128 for encoding.
    pub ub_va: u128,
    pub home_node_id: u16,
    pub device_id: u16,
    pub mr_handle: u32,
    pub base_offset: u64,
    pub error_code: u32,
}

impl AllocRespPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(40);
        buf.put_u64(self.region_id);
        buf.put_u128(self.ub_va);
        buf.put_u16(self.home_node_id);
        buf.put_u16(self.device_id);
        buf.put_u32(self.mr_handle);
        buf.put_u64(self.base_offset);
        buf.put_u32(self.error_code);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let region_id = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let ub_va = cur.read_u128::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let home_node_id = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let device_id = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let mr_handle = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let base_offset = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let error_code = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(AllocRespPayload {
            region_id, ub_va, home_node_id, device_id, mr_handle, base_offset, error_code,
        })
    }
}

/// REGION_CREATE message payload (FR-GVA-3): placer → home node.
#[derive(Debug, Clone)]
pub struct RegionCreatePayload {
    pub region_id: u64,
    pub size: u64,
    pub device_kind: DeviceKind,
}

impl RegionCreatePayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(17);
        buf.put_u64(self.region_id);
        buf.put_u64(self.size);
        buf.put_u8(self.device_kind as u8);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let region_id = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let size = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let kind_u8 = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
        let device_kind = DeviceKind::from_u8(kind_u8)
            .ok_or_else(|| UbError::Internal(format!("invalid device_kind: {kind_u8}")))?;
        Ok(RegionCreatePayload { region_id, size, device_kind })
    }
}

/// REGION_CREATE_OK message payload (FR-GVA-3): home node → placer.
#[derive(Debug, Clone)]
pub struct RegionCreateOkPayload {
    pub region_id: u64,
    pub mr_handle: u32,
    pub base_offset: u64,
    pub device_id: u16,
}

impl RegionCreateOkPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(22);
        buf.put_u64(self.region_id);
        buf.put_u32(self.mr_handle);
        buf.put_u64(self.base_offset);
        buf.put_u16(self.device_id);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let region_id = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let mr_handle = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let base_offset = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let device_id = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(RegionCreateOkPayload { region_id, mr_handle, base_offset, device_id })
    }
}

/// REGION_DELETE message payload (FR-GVA-5): placer → home node.
#[derive(Debug, Clone)]
pub struct RegionDeletePayload {
    pub region_id: u64,
}

impl RegionDeletePayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8);
        buf.put_u64(self.region_id);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let region_id = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(RegionDeletePayload { region_id })
    }
}

/// FETCH message payload (FR-GVA-4): reader → home node.
#[derive(Debug, Clone)]
pub struct FetchPayload {
    pub region_id: u64,
    pub offset: u64,
    pub len: u32,
    pub requester_node_id: u16,
}

impl FetchPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(22);
        buf.put_u64(self.region_id);
        buf.put_u64(self.offset);
        buf.put_u32(self.len);
        buf.put_u16(self.requester_node_id);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let region_id = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let offset = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let len = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let requester_node_id = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(FetchPayload { region_id, offset, len, requester_node_id })
    }
}

/// FETCH_RESP message payload (FR-GVA-4): home node → reader.
#[derive(Debug, Clone)]
pub struct FetchRespPayload {
    pub region_id: u64,
    pub epoch: u64,
    pub payload: Vec<u8>,
    pub error_code: u32,
}

impl FetchRespPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(20 + self.payload.len());
        buf.put_u64(self.region_id);
        buf.put_u64(self.epoch);
        buf.put_u32(self.payload.len() as u32);
        buf.put_slice(&self.payload);
        buf.put_u32(self.error_code);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let region_id = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let epoch = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let payload_len = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))? as usize;
        let mut payload = vec![0u8; payload_len];
        cur.read_exact(&mut payload).map_err(|e| UbError::Internal(e.to_string()))?;
        let error_code = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(FetchRespPayload { region_id, epoch, payload, error_code })
    }
}

/// WRITE_LOCK_REQ message payload (FR-GVA-4): writer → home node.
#[derive(Debug, Clone)]
pub struct WriteLockReqPayload {
    pub region_id: u64,
    pub writer_node_id: u16,
}

impl WriteLockReqPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(10);
        buf.put_u64(self.region_id);
        buf.put_u16(self.writer_node_id);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let region_id = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let writer_node_id = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(WriteLockReqPayload { region_id, writer_node_id })
    }
}

/// WRITE_LOCK_GRANTED message payload (FR-GVA-4): home node → writer.
#[derive(Debug, Clone)]
pub struct WriteLockGrantedPayload {
    pub region_id: u64,
    pub epoch: u64,
    pub error_code: u32,
}

impl WriteLockGrantedPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(20);
        buf.put_u64(self.region_id);
        buf.put_u64(self.epoch);
        buf.put_u32(self.error_code);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let region_id = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let epoch = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let error_code = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(WriteLockGrantedPayload { region_id, epoch, error_code })
    }
}

/// WRITE_UNLOCK message payload (FR-GVA-4): writer → home node.
#[derive(Debug, Clone)]
pub struct WriteUnlockPayload {
    pub region_id: u64,
    pub writer_node_id: u16,
}

impl WriteUnlockPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(10);
        buf.put_u64(self.region_id);
        buf.put_u16(self.writer_node_id);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let region_id = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let writer_node_id = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(WriteUnlockPayload { region_id, writer_node_id })
    }
}

/// INVALIDATE message payload (FR-GVA-4): home node → readers.
#[derive(Debug, Clone)]
pub struct InvalidatePayload {
    pub region_id: u64,
    pub new_epoch: u64,
}

impl InvalidatePayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(16);
        buf.put_u64(self.region_id);
        buf.put_u64(self.new_epoch);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let region_id = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        let new_epoch = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(InvalidatePayload { region_id, new_epoch })
    }
}

/// INVALIDATE_ACK message payload (FR-GVA-4): reader → home node.
#[derive(Debug, Clone)]
pub struct InvalidateAckPayload {
    pub region_id: u64,
}

impl InvalidateAckPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8);
        buf.put_u64(self.region_id);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, UbError> {
        let mut cur = Cursor::new(data);
        let region_id = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
        Ok(InvalidateAckPayload { region_id })
    }
}

/// Control message enum — all control plane messages.
#[derive(Debug, Clone)]
pub enum ControlMsg {
    Hello(HelloPayload),
    HelloAck(HelloPayload),
    MemberUp(HelloPayload), // Same payload as Hello — carries node info
    MemberDown(MemberDownPayload),
    MemberSnapshot(MemberSnapshotPayload),
    Heartbeat(HeartbeatPayload),
    HeartbeatAck(HeartbeatPayload),
    MrPublish(MrPublishPayload),
    MrRevoke(MrRevokePayload),
    Join(JoinPayload),
    DeviceProfilePublish(DeviceProfilePublishPayload),
    AllocReq(AllocReqPayload),
    AllocResp(AllocRespPayload),
    RegionCreate(RegionCreatePayload),
    RegionCreateOk(RegionCreateOkPayload),
    RegionDelete(RegionDeletePayload),
    Fetch(FetchPayload),
    FetchResp(FetchRespPayload),
    WriteLockReq(WriteLockReqPayload),
    WriteLockGranted(WriteLockGrantedPayload),
    WriteUnlock(WriteUnlockPayload),
    Invalidate(InvalidatePayload),
    InvalidateAck(InvalidateAckPayload),
}

impl ControlMsg {
    pub fn msg_type(&self) -> CtrlMsgType {
        match self {
            ControlMsg::Hello(_) => CtrlMsgType::Hello,
            ControlMsg::HelloAck(_) => CtrlMsgType::HelloAck,
            ControlMsg::MemberUp(_) => CtrlMsgType::MemberUp,
            ControlMsg::MemberDown(_) => CtrlMsgType::MemberDown,
            ControlMsg::MemberSnapshot(_) => CtrlMsgType::MemberSnapshot,
            ControlMsg::Heartbeat(_) => CtrlMsgType::Heartbeat,
            ControlMsg::HeartbeatAck(_) => CtrlMsgType::HeartbeatAck,
            ControlMsg::MrPublish(_) => CtrlMsgType::MrPublish,
            ControlMsg::MrRevoke(_) => CtrlMsgType::MrRevoke,
            ControlMsg::Join(_) => CtrlMsgType::Join,
            ControlMsg::DeviceProfilePublish(_) => CtrlMsgType::DeviceProfilePublish,
            ControlMsg::AllocReq(_) => CtrlMsgType::AllocReq,
            ControlMsg::AllocResp(_) => CtrlMsgType::AllocResp,
            ControlMsg::RegionCreate(_) => CtrlMsgType::RegionCreate,
            ControlMsg::RegionCreateOk(_) => CtrlMsgType::RegionCreateOk,
            ControlMsg::RegionDelete(_) => CtrlMsgType::RegionDelete,
            ControlMsg::Fetch(_) => CtrlMsgType::Fetch,
            ControlMsg::FetchResp(_) => CtrlMsgType::FetchResp,
            ControlMsg::WriteLockReq(_) => CtrlMsgType::WriteLockReq,
            ControlMsg::WriteLockGranted(_) => CtrlMsgType::WriteLockGranted,
            ControlMsg::WriteUnlock(_) => CtrlMsgType::WriteUnlock,
            ControlMsg::Invalidate(_) => CtrlMsgType::Invalidate,
            ControlMsg::InvalidateAck(_) => CtrlMsgType::InvalidateAck,
        }
    }

    pub fn encode_payload(&self) -> Vec<u8> {
        match self {
            ControlMsg::Hello(p) => p.encode(),
            ControlMsg::HelloAck(p) => p.encode(),
            ControlMsg::MemberUp(p) => p.encode(),
            ControlMsg::MemberDown(p) => p.encode(),
            ControlMsg::MemberSnapshot(p) => p.encode(),
            ControlMsg::Heartbeat(p) => p.encode(),
            ControlMsg::HeartbeatAck(p) => p.encode(),
            ControlMsg::MrPublish(p) => p.encode(),
            ControlMsg::MrRevoke(p) => p.encode(),
            ControlMsg::Join(p) => p.encode(),
            ControlMsg::DeviceProfilePublish(p) => p.encode(),
            ControlMsg::AllocReq(p) => p.encode(),
            ControlMsg::AllocResp(p) => p.encode(),
            ControlMsg::RegionCreate(p) => p.encode(),
            ControlMsg::RegionCreateOk(p) => p.encode(),
            ControlMsg::RegionDelete(p) => p.encode(),
            ControlMsg::Fetch(p) => p.encode(),
            ControlMsg::FetchResp(p) => p.encode(),
            ControlMsg::WriteLockReq(p) => p.encode(),
            ControlMsg::WriteLockGranted(p) => p.encode(),
            ControlMsg::WriteUnlock(p) => p.encode(),
            ControlMsg::Invalidate(p) => p.encode(),
            ControlMsg::InvalidateAck(p) => p.encode(),
        }
    }

    /// Encode a complete control message (framing + payload).
    pub fn encode(&self) -> BytesMut {
        let payload = self.encode_payload();
        encode_ctrl_message(self.msg_type(), &payload)
    }

    /// Decode a control message from framed bytes.
    pub fn decode(buf: &[u8]) -> Result<Self, UbError> {
        let (msg_type, payload) = decode_ctrl_message(buf)?;
        match msg_type {
            CtrlMsgType::Hello => Ok(ControlMsg::Hello(HelloPayload::decode(&payload)?)),
            CtrlMsgType::HelloAck => Ok(ControlMsg::HelloAck(HelloPayload::decode(&payload)?)),
            CtrlMsgType::MemberUp => Ok(ControlMsg::MemberUp(HelloPayload::decode(&payload)?)),
            CtrlMsgType::MemberDown => Ok(ControlMsg::MemberDown(MemberDownPayload::decode(&payload)?)),
            CtrlMsgType::MemberSnapshot => Ok(ControlMsg::MemberSnapshot(MemberSnapshotPayload::decode(&payload)?)),
            CtrlMsgType::Heartbeat => Ok(ControlMsg::Heartbeat(HeartbeatPayload::decode(&payload)?)),
            CtrlMsgType::HeartbeatAck => Ok(ControlMsg::HeartbeatAck(HeartbeatPayload::decode(&payload)?)),
            CtrlMsgType::MrPublish => Ok(ControlMsg::MrPublish(MrPublishPayload::decode(&payload)?)),
            CtrlMsgType::MrRevoke => Ok(ControlMsg::MrRevoke(MrRevokePayload::decode(&payload)?)),
            CtrlMsgType::Join => Ok(ControlMsg::Join(JoinPayload::decode(&payload)?)),
            CtrlMsgType::DeviceProfilePublish => Ok(ControlMsg::DeviceProfilePublish(DeviceProfilePublishPayload::decode(&payload)?)),
            CtrlMsgType::AllocReq => Ok(ControlMsg::AllocReq(AllocReqPayload::decode(&payload)?)),
            CtrlMsgType::AllocResp => Ok(ControlMsg::AllocResp(AllocRespPayload::decode(&payload)?)),
            CtrlMsgType::RegionCreate => Ok(ControlMsg::RegionCreate(RegionCreatePayload::decode(&payload)?)),
            CtrlMsgType::RegionCreateOk => Ok(ControlMsg::RegionCreateOk(RegionCreateOkPayload::decode(&payload)?)),
            CtrlMsgType::RegionDelete => Ok(ControlMsg::RegionDelete(RegionDeletePayload::decode(&payload)?)),
            CtrlMsgType::Fetch => Ok(ControlMsg::Fetch(FetchPayload::decode(&payload)?)),
            CtrlMsgType::FetchResp => Ok(ControlMsg::FetchResp(FetchRespPayload::decode(&payload)?)),
            CtrlMsgType::WriteLockReq => Ok(ControlMsg::WriteLockReq(WriteLockReqPayload::decode(&payload)?)),
            CtrlMsgType::WriteLockGranted => Ok(ControlMsg::WriteLockGranted(WriteLockGrantedPayload::decode(&payload)?)),
            CtrlMsgType::WriteUnlock => Ok(ControlMsg::WriteUnlock(WriteUnlockPayload::decode(&payload)?)),
            CtrlMsgType::Invalidate => Ok(ControlMsg::Invalidate(InvalidatePayload::decode(&payload)?)),
            CtrlMsgType::InvalidateAck => Ok(ControlMsg::InvalidateAck(InvalidateAckPayload::decode(&payload)?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ctrl_framing_roundtrip() {
        let payload = vec![1, 2, 3, 4];
        let encoded = encode_ctrl_message(CtrlMsgType::Hello, &payload);
        let (msg_type, decoded_payload) = decode_ctrl_message(&encoded).unwrap();
        assert_eq!(msg_type, CtrlMsgType::Hello);
        assert_eq!(decoded_payload, payload);
    }

    #[test]
    fn test_hello_roundtrip() {
        let hello = HelloPayload {
            node_id: 42,
            version: 1,
            local_epoch: 12345,
            initial_credits: 64,
            data_addr: "10.0.0.1:7901".to_string(),
        };
        let encoded = hello.encode();
        let decoded = HelloPayload::decode(&encoded).unwrap();
        assert_eq!(decoded.node_id, 42);
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.local_epoch, 12345);
        assert_eq!(decoded.initial_credits, 64);
        assert_eq!(decoded.data_addr, "10.0.0.1:7901");
    }

    #[test]
    fn test_heartbeat_roundtrip() {
        let hb = HeartbeatPayload { node_id: 42, timestamp: 1234567890 };
        let encoded = hb.encode();
        let decoded = HeartbeatPayload::decode(&encoded).unwrap();
        assert_eq!(decoded.node_id, 42);
        assert_eq!(decoded.timestamp, 1234567890);
    }

    #[test]
    fn test_mr_publish_roundtrip() {
        let mr = MrPublishPayload {
            owner_node: 1,
            mr_handle: 5,
            base_ub_addr: UbAddr::new(1, 1, 0, 0, 0),
            len: 1024,
            perms: MrPerms::READ | MrPerms::WRITE,
            device_kind: DeviceKind::Memory,
        };
        let encoded = mr.encode();
        let decoded = MrPublishPayload::decode(&encoded).unwrap();
        assert_eq!(decoded.owner_node, 1);
        assert_eq!(decoded.mr_handle, 5);
        assert_eq!(decoded.len, 1024);
        assert_eq!(decoded.perms, MrPerms::READ | MrPerms::WRITE);
        assert_eq!(decoded.device_kind, DeviceKind::Memory);
    }

    #[test]
    fn test_mr_revoke_roundtrip() {
        let revoke = MrRevokePayload {
            owner_node: 1,
            mr_handle: 5,
        };
        let encoded = revoke.encode();
        let decoded = MrRevokePayload::decode(&encoded).unwrap();
        assert_eq!(decoded.owner_node, 1);
        assert_eq!(decoded.mr_handle, 5);
    }

    #[test]
    fn test_control_msg_full_roundtrip() {
        let msg = ControlMsg::Hello(HelloPayload {
            node_id: 42,
            version: 1,
            local_epoch: 999,
            initial_credits: 32,
            data_addr: "192.168.1.1:7901".to_string(),
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::Hello(h) = decoded {
            assert_eq!(h.node_id, 42);
            assert_eq!(h.initial_credits, 32);
            assert_eq!(h.data_addr, "192.168.1.1:7901");
        } else {
            panic!("expected Hello");
        }
    }

    #[test]
    fn test_member_down_roundtrip() {
        let msg = ControlMsg::MemberDown(MemberDownPayload {
            node_id: 5,
            reason: 1,
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::MemberDown(d) = decoded {
            assert_eq!(d.node_id, 5);
            assert_eq!(d.reason, 1);
        } else {
            panic!("expected MemberDown");
        }
    }

    #[test]
    fn test_join_payload_roundtrip() {
        let join = JoinPayload {
            node_id: 10,
            epoch: 555,
            data_addr: "10.0.0.10:7901".to_string(),
            control_addr: "10.0.0.10:7900".to_string(),
            initial_credits: 32,
        };
        let encoded = join.encode();
        let decoded = JoinPayload::decode(&encoded).unwrap();
        assert_eq!(decoded.node_id, 10);
        assert_eq!(decoded.epoch, 555);
        assert_eq!(decoded.data_addr, "10.0.0.10:7901");
        assert_eq!(decoded.control_addr, "10.0.0.10:7900");
        assert_eq!(decoded.initial_credits, 32);
    }

    #[test]
    fn test_member_snapshot_roundtrip() {
        let snapshot = MemberSnapshotPayload {
            nodes: vec![
                SnapshotNodeInfo {
                    node_id: 1,
                    state: 1, // Active
                    data_addr: "10.0.0.1:7901".to_string(),
                    control_addr: "10.0.0.1:7900".to_string(),
                    epoch: 100,
                },
                SnapshotNodeInfo {
                    node_id: 2,
                    state: 1,
                    data_addr: "10.0.0.2:7901".to_string(),
                    control_addr: "10.0.0.2:7900".to_string(),
                    epoch: 200,
                },
            ],
        };
        let encoded = snapshot.encode();
        let decoded = MemberSnapshotPayload::decode(&encoded).unwrap();
        assert_eq!(decoded.nodes.len(), 2);
        assert_eq!(decoded.nodes[0].node_id, 1);
        assert_eq!(decoded.nodes[0].state, 1);
        assert_eq!(decoded.nodes[0].data_addr, "10.0.0.1:7901");
        assert_eq!(decoded.nodes[0].control_addr, "10.0.0.1:7900");
        assert_eq!(decoded.nodes[0].epoch, 100);
        assert_eq!(decoded.nodes[1].node_id, 2);
        assert_eq!(decoded.nodes[1].epoch, 200);
    }

    #[test]
    fn test_join_control_msg_roundtrip() {
        let msg = ControlMsg::Join(JoinPayload {
            node_id: 5,
            epoch: 42,
            data_addr: "192.168.1.5:7901".to_string(),
            control_addr: "192.168.1.5:7900".to_string(),
            initial_credits: 64,
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::Join(j) = decoded {
            assert_eq!(j.node_id, 5);
            assert_eq!(j.epoch, 42);
            assert_eq!(j.data_addr, "192.168.1.5:7901");
            assert_eq!(j.control_addr, "192.168.1.5:7900");
            assert_eq!(j.initial_credits, 64);
        } else {
            panic!("expected Join");
        }
    }

    #[test]
    fn test_member_snapshot_control_msg_roundtrip() {
        let msg = ControlMsg::MemberSnapshot(MemberSnapshotPayload {
            nodes: vec![SnapshotNodeInfo {
                node_id: 3,
                state: 1,
                data_addr: "10.0.0.3:7901".to_string(),
                control_addr: "10.0.0.3:7900".to_string(),
                epoch: 300,
            }],
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::MemberSnapshot(s) = decoded {
            assert_eq!(s.nodes.len(), 1);
            assert_eq!(s.nodes[0].node_id, 3);
        } else {
            panic!("expected MemberSnapshot");
        }
    }

    #[test]
    fn test_device_profile_entry_roundtrip() {
        let entry = DeviceProfileEntry {
            node_id: 1,
            device_id: 0,
            kind: DeviceKind::Memory,
            tier: StorageTier::Warm,
            capacity_bytes: 1024 * 1024 * 256,
            peak_read_bw_mbps: 10_000,
            peak_write_bw_mbps: 10_000,
            read_latency_ns_p50: 200,
            write_latency_ns_p50: 200,
            used_bytes: 1024 * 1024 * 50,
            recent_rps: 1234,
        };
        let encoded = entry.encode();
        let decoded = DeviceProfileEntry::decode(&encoded).unwrap();
        assert_eq!(decoded.node_id, 1);
        assert_eq!(decoded.device_id, 0);
        assert_eq!(decoded.kind, DeviceKind::Memory);
        assert_eq!(decoded.tier, StorageTier::Warm);
        assert_eq!(decoded.capacity_bytes, 1024 * 1024 * 256);
        assert_eq!(decoded.peak_read_bw_mbps, 10_000);
        assert_eq!(decoded.read_latency_ns_p50, 200);
        assert_eq!(decoded.used_bytes, 1024 * 1024 * 50);
        assert_eq!(decoded.recent_rps, 1234);
    }

    #[test]
    fn test_device_profile_publish_roundtrip() {
        let msg = ControlMsg::DeviceProfilePublish(DeviceProfilePublishPayload {
            entries: vec![
                DeviceProfileEntry {
                    node_id: 1,
                    device_id: 0,
                    kind: DeviceKind::Memory,
                    tier: StorageTier::Warm,
                    capacity_bytes: 1024 * 1024 * 512,
                    peak_read_bw_mbps: 10_000,
                    peak_write_bw_mbps: 10_000,
                    read_latency_ns_p50: 200,
                    write_latency_ns_p50: 200,
                    used_bytes: 0,
                    recent_rps: 0,
                },
                DeviceProfileEntry {
                    node_id: 1,
                    device_id: 1,
                    kind: DeviceKind::Npu,
                    tier: StorageTier::Hot,
                    capacity_bytes: 1024 * 1024 * 256,
                    peak_read_bw_mbps: 50_000,
                    peak_write_bw_mbps: 50_000,
                    read_latency_ns_p50: 50,
                    write_latency_ns_p50: 50,
                    used_bytes: 1024 * 1024 * 10,
                    recent_rps: 500,
                },
            ],
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::DeviceProfilePublish(p) = decoded {
            assert_eq!(p.entries.len(), 2);
            assert_eq!(p.entries[0].kind, DeviceKind::Memory);
            assert_eq!(p.entries[0].tier, StorageTier::Warm);
            assert_eq!(p.entries[1].kind, DeviceKind::Npu);
            assert_eq!(p.entries[1].tier, StorageTier::Hot);
            assert_eq!(p.entries[1].peak_read_bw_mbps, 50_000);
        } else {
            panic!("expected DeviceProfilePublish");
        }
    }

    #[test]
    fn test_alloc_req_roundtrip() {
        let msg = ControlMsg::AllocReq(AllocReqPayload {
            region_id: 42,
            size: 4096,
            access: AccessPattern::ReadHeavy,
            latency_class: LatencyClass::Critical,
            capacity_class: CapacityClass::Small,
            pin_kind: 1,
            expected_readers: 5,
            requester_node_id: 3,
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::AllocReq(p) = decoded {
            assert_eq!(p.region_id, 42);
            assert_eq!(p.size, 4096);
            assert_eq!(p.access, AccessPattern::ReadHeavy);
            assert_eq!(p.latency_class, LatencyClass::Critical);
            assert_eq!(p.capacity_class, CapacityClass::Small);
            assert_eq!(p.pin_kind, 1);
            assert_eq!(p.expected_readers, 5);
            assert_eq!(p.requester_node_id, 3);
        } else {
            panic!("expected AllocReq");
        }
    }

    #[test]
    fn test_alloc_resp_roundtrip() {
        let msg = ControlMsg::AllocResp(AllocRespPayload {
            region_id: 42,
            ub_va: ub_core::types::UbVa::new(ub_core::types::RegionId(42), 0).0,
            home_node_id: 1,
            device_id: 0,
            mr_handle: 5,
            base_offset: 1024,
            error_code: 0,
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::AllocResp(p) = decoded {
            assert_eq!(p.region_id, 42);
            assert_eq!(p.home_node_id, 1);
            assert_eq!(p.mr_handle, 5);
            assert_eq!(p.base_offset, 1024);
            assert_eq!(p.error_code, 0);
        } else {
            panic!("expected AllocResp");
        }
    }

    #[test]
    fn test_region_create_roundtrip() {
        let msg = ControlMsg::RegionCreate(RegionCreatePayload {
            region_id: 100,
            size: 8192,
            device_kind: DeviceKind::Memory,
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::RegionCreate(p) = decoded {
            assert_eq!(p.region_id, 100);
            assert_eq!(p.size, 8192);
            assert_eq!(p.device_kind, DeviceKind::Memory);
        } else {
            panic!("expected RegionCreate");
        }
    }

    #[test]
    fn test_region_create_ok_roundtrip() {
        let msg = ControlMsg::RegionCreateOk(RegionCreateOkPayload {
            region_id: 100,
            mr_handle: 7,
            base_offset: 2048,
            device_id: 0,
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::RegionCreateOk(p) = decoded {
            assert_eq!(p.region_id, 100);
            assert_eq!(p.mr_handle, 7);
            assert_eq!(p.base_offset, 2048);
            assert_eq!(p.device_id, 0);
        } else {
            panic!("expected RegionCreateOk");
        }
    }

    #[test]
    fn test_region_delete_roundtrip() {
        let msg = ControlMsg::RegionDelete(RegionDeletePayload {
            region_id: 100,
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::RegionDelete(p) = decoded {
            assert_eq!(p.region_id, 100);
        } else {
            panic!("expected RegionDelete");
        }
    }

    #[test]
    fn test_fetch_roundtrip() {
        let msg = ControlMsg::Fetch(FetchPayload {
            region_id: 42,
            offset: 1024,
            len: 4096,
            requester_node_id: 3,
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::Fetch(p) = decoded {
            assert_eq!(p.region_id, 42);
            assert_eq!(p.offset, 1024);
            assert_eq!(p.len, 4096);
            assert_eq!(p.requester_node_id, 3);
        } else {
            panic!("expected Fetch");
        }
    }

    #[test]
    fn test_fetch_resp_roundtrip() {
        let msg = ControlMsg::FetchResp(FetchRespPayload {
            region_id: 42,
            epoch: 7,
            payload: vec![0xAB, 0xCD, 0xEF, 0x01, 0x23],
            error_code: 0,
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::FetchResp(p) = decoded {
            assert_eq!(p.region_id, 42);
            assert_eq!(p.epoch, 7);
            assert_eq!(p.payload, vec![0xAB, 0xCD, 0xEF, 0x01, 0x23]);
            assert_eq!(p.error_code, 0);
        } else {
            panic!("expected FetchResp");
        }
    }

    #[test]
    fn test_write_lock_req_roundtrip() {
        let msg = ControlMsg::WriteLockReq(WriteLockReqPayload {
            region_id: 42,
            writer_node_id: 3,
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::WriteLockReq(p) = decoded {
            assert_eq!(p.region_id, 42);
            assert_eq!(p.writer_node_id, 3);
        } else {
            panic!("expected WriteLockReq");
        }
    }

    #[test]
    fn test_write_lock_granted_roundtrip() {
        let msg = ControlMsg::WriteLockGranted(WriteLockGrantedPayload {
            region_id: 42,
            epoch: 7,
            error_code: 0,
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::WriteLockGranted(p) = decoded {
            assert_eq!(p.region_id, 42);
            assert_eq!(p.epoch, 7);
            assert_eq!(p.error_code, 0);
        } else {
            panic!("expected WriteLockGranted");
        }
    }

    #[test]
    fn test_write_unlock_roundtrip() {
        let msg = ControlMsg::WriteUnlock(WriteUnlockPayload {
            region_id: 42,
            writer_node_id: 3,
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::WriteUnlock(p) = decoded {
            assert_eq!(p.region_id, 42);
            assert_eq!(p.writer_node_id, 3);
        } else {
            panic!("expected WriteUnlock");
        }
    }

    #[test]
    fn test_invalidate_roundtrip() {
        let msg = ControlMsg::Invalidate(InvalidatePayload {
            region_id: 42,
            new_epoch: 8,
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::Invalidate(p) = decoded {
            assert_eq!(p.region_id, 42);
            assert_eq!(p.new_epoch, 8);
        } else {
            panic!("expected Invalidate");
        }
    }

    #[test]
    fn test_invalidate_ack_roundtrip() {
        let msg = ControlMsg::InvalidateAck(InvalidateAckPayload {
            region_id: 42,
        });
        let encoded = msg.encode();
        let decoded = ControlMsg::decode(&encoded).unwrap();
        if let ControlMsg::InvalidateAck(p) = decoded {
            assert_eq!(p.region_id, 42);
        } else {
            panic!("expected InvalidateAck");
        }
    }
}
