use byteorder::{BigEndian, ReadBytesExt};
use bytes::{BufMut, BytesMut};
use std::io::{Cursor, Read};

use ub_core::addr::UbAddr;
use ub_core::error::UbError;
use ub_core::types::{DeviceKind, MrPerms};

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
}
