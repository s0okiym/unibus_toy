use byteorder::{BigEndian, ReadBytesExt};
use bytes::{BufMut, BytesMut};
use std::io::{Cursor, Read};

use ub_core::addr::UbAddr;
use ub_core::error::UbError;
use ub_core::types::Verb;

use crate::frame::*;

/// Encode a complete frame (generic header + optional DATA ext header + payload).
pub fn encode_frame(
    header: &FrameHeader,
    ext: Option<&DataExtHeader>,
    payload: &[u8],
) -> BytesMut {
    let ext_size = if let Some(ext) = ext {
        if ext.imm.is_some() {
            DATA_EXT_HEADER_IMM_SIZE
        } else {
            DATA_EXT_HEADER_SIZE
        }
    } else {
        0
    };
    let total = FRAME_HEADER_SIZE + ext_size + payload.len();
    let mut buf = BytesMut::with_capacity(total);

    // Generic header (32 bytes)
    buf.put_u32(header.magic);
    buf.put_u8(header.version);
    buf.put_u8(header.frame_type as u8);
    buf.put_u16(header.flags.bits());
    buf.put_u16(header.src_node);
    buf.put_u16(header.dst_node);
    buf.put_u32(header.reserved);
    buf.put_u64(header.stream_seq);
    buf.put_u32(header.payload_len);
    buf.put_u32(header.header_crc);

    // DATA ext header
    if let Some(ext) = ext {
        buf.put_u8(ext.verb as u8);
        buf.put_u8(ext.ext_flags.bits());
        buf.put_u16(0); // reserved
        buf.put_u32(ext.mr_handle);
        buf.put_u32(ext.jetty_src);
        buf.put_u32(ext.jetty_dst);
        buf.put_u64(ext.opaque);
        buf.put_u32(ext.frag_id);
        buf.put_u16(ext.frag_index);
        buf.put_u16(ext.frag_total);
        buf.put_slice(&ext.ub_addr.to_bytes());
        if let Some(imm) = ext.imm {
            buf.put_u64(imm);
        }
    }

    buf.put_slice(payload);
    buf
}

/// Decode a complete frame from raw bytes.
/// Returns (FrameHeader, Option<DataExtHeader>, payload slice).
pub fn decode_frame(buf: &[u8]) -> Result<(FrameHeader, Option<DataExtHeader>, &[u8]), UbError> {
    if buf.len() < FRAME_HEADER_SIZE {
        return Err(UbError::Internal("frame too short for header".into()));
    }

    let mut cur = Cursor::new(buf);
    let magic = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    if magic != MAGIC {
        return Err(UbError::Internal(format!("invalid magic: 0x{magic:08x}")));
    }

    let version = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
    if version != VERSION {
        return Err(UbError::Internal(format!("version mismatch: {version}")));
    }

    let frame_type_u8 = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
    let frame_type = FrameType::from_u8(frame_type_u8)
        .ok_or_else(|| UbError::Internal(format!("unknown frame type: {frame_type_u8}")))?;

    let flags_u16 = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let flags = FrameFlags::from_bits_truncate(flags_u16);

    let src_node = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let dst_node = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let reserved = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let stream_seq = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let payload_len = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let header_crc = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;

    let header = FrameHeader {
        magic,
        version,
        frame_type,
        flags,
        src_node,
        dst_node,
        reserved,
        stream_seq,
        payload_len,
        header_crc,
    };

    let rest = &buf[FRAME_HEADER_SIZE..];

    // Decode DATA ext header if frame type is Data
    let ext = if frame_type == FrameType::Data {
        Some(decode_data_ext_header(rest)?)
    } else {
        None
    };

    let ext_size = if ext.is_some() {
        if ext.as_ref().unwrap().imm.is_some() {
            DATA_EXT_HEADER_IMM_SIZE
        } else {
            DATA_EXT_HEADER_SIZE
        }
    } else {
        0
    };

    let payload = &buf[FRAME_HEADER_SIZE + ext_size..];
    Ok((header, ext, payload))
}

fn decode_data_ext_header(buf: &[u8]) -> Result<DataExtHeader, UbError> {
    if buf.len() < DATA_EXT_HEADER_SIZE {
        return Err(UbError::Internal("DATA ext header too short".into()));
    }

    let mut cur = Cursor::new(buf);
    let verb_u8 = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
    let verb = Verb::from_u8(verb_u8)
        .ok_or_else(|| UbError::Internal(format!("unknown verb: {verb_u8}")))?;

    let ext_flags_u8 = cur.read_u8().map_err(|e| UbError::Internal(e.to_string()))?;
    let ext_flags = ExtFlags::from_bits_truncate(ext_flags_u8);

    let _reserved = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let mr_handle = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let jetty_src = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let jetty_dst = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let opaque = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let frag_id = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let frag_index = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let frag_total = cur.read_u16::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;

    let mut addr_bytes = [0u8; 16];
    cur.read_exact(&mut addr_bytes).map_err(|e| UbError::Internal(e.to_string()))?;
    let ub_addr = UbAddr::from_bytes(&addr_bytes);

    let imm = if ext_flags.contains(ExtFlags::HAS_IMM) {
        Some(cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?)
    } else {
        None
    };

    Ok(DataExtHeader {
        verb,
        ext_flags,
        mr_handle,
        jetty_src,
        jetty_dst,
        opaque,
        frag_id,
        frag_index,
        frag_total,
        ub_addr,
        imm,
    })
}

/// Encode ACK payload.
pub fn encode_ack_payload(ack: &AckPayload) -> BytesMut {
    let mut buf = BytesMut::with_capacity(48);
    buf.put_u64(ack.ack_seq);
    buf.put_u32(ack.credit_grant);
    buf.put_u32(ack.reserved);
    if let Some(ref bitmap) = ack.sack_bitmap {
        buf.put_slice(bitmap.as_slice());
    }
    buf
}

/// Decode ACK payload.
pub fn decode_ack_payload(buf: &[u8], has_sack: bool) -> Result<AckPayload, UbError> {
    let min_len = 16;
    if buf.len() < min_len {
        return Err(UbError::Internal("ACK payload too short".into()));
    }
    let mut cur = Cursor::new(buf);
    let ack_seq = cur.read_u64::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let credit_grant = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let reserved = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;

    let sack_bitmap = if has_sack {
        if buf.len() < 48 {
            return Err(UbError::Internal("ACK payload too short for SACK".into()));
        }
        let mut bitmap = [0u8; 32];
        cur.read_exact(&mut bitmap).map_err(|e| UbError::Internal(e.to_string()))?;
        Some(bitmap)
    } else {
        None
    };

    Ok(AckPayload {
        ack_seq,
        credit_grant,
        reserved,
        sack_bitmap,
    })
}

/// Encode CREDIT payload.
pub fn encode_credit_payload(credit: &CreditPayload) -> BytesMut {
    let mut buf = BytesMut::with_capacity(8);
    buf.put_u32(credit.credits);
    buf.put_u32(credit.reserved);
    buf
}

/// Decode CREDIT payload.
pub fn decode_credit_payload(buf: &[u8]) -> Result<CreditPayload, UbError> {
    if buf.len() < 8 {
        return Err(UbError::Internal("CREDIT payload too short".into()));
    }
    let mut cur = Cursor::new(buf);
    let credits = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    let reserved = cur.read_u32::<BigEndian>().map_err(|e| UbError::Internal(e.to_string()))?;
    Ok(CreditPayload { credits, reserved })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_header() -> FrameHeader {
        FrameHeader {
            magic: MAGIC,
            version: VERSION,
            frame_type: FrameType::Data,
            flags: FrameFlags::ACK_REQ,
            src_node: 1,
            dst_node: 2,
            reserved: 0,
            stream_seq: 42,
            payload_len: 4,
            header_crc: 0,
        }
    }

    fn make_test_ext() -> DataExtHeader {
        DataExtHeader {
            verb: Verb::Write,
            ext_flags: ExtFlags::empty(),
            mr_handle: 1,
            jetty_src: 0,
            jetty_dst: 0,
            opaque: 100,
            frag_id: 100,
            frag_index: 0,
            frag_total: 1,
            ub_addr: UbAddr::new(1, 2, 0, 0, 0),
            imm: None,
        }
    }

    #[test]
    fn test_data_frame_roundtrip() {
        let header = make_test_header();
        let ext = make_test_ext();
        let payload = &[1u8, 2, 3, 4];

        let encoded = encode_frame(&header, Some(&ext), payload);
        let (h, e, p) = decode_frame(&encoded).unwrap();

        assert_eq!(h, header);
        assert_eq!(e.as_ref().unwrap(), &ext);
        assert_eq!(p, payload);
    }

    #[test]
    fn test_data_frame_with_imm() {
        let mut ext = make_test_ext();
        ext.ext_flags = ExtFlags::HAS_IMM;
        ext.imm = Some(0x1234_5678);

        let header = make_test_header();
        let encoded = encode_frame(&header, Some(&ext), &[0u8; 4]);
        let (h, e, p) = decode_frame(&encoded).unwrap();

        assert_eq!(e.unwrap().imm, Some(0x1234_5678));
    }

    #[test]
    fn test_fragment_frame() {
        let mut ext = make_test_ext();
        ext.ext_flags = ExtFlags::FRAG;
        ext.frag_index = 1;
        ext.frag_total = 3;

        let header = make_test_header();
        let encoded = encode_frame(&header, Some(&ext), &[]);
        let (_, e, _) = decode_frame(&encoded).unwrap();
        let decoded_ext = e.unwrap();
        assert_eq!(decoded_ext.frag_index, 1);
        assert_eq!(decoded_ext.frag_total, 3);
    }

    #[test]
    fn test_invalid_magic() {
        let mut header = make_test_header();
        header.magic = 0xDEAD_BEEF;
        let encoded = encode_frame(&header, None, &[]);
        assert!(decode_frame(&encoded).is_err());
    }

    #[test]
    fn test_version_mismatch() {
        let mut header = make_test_header();
        header.version = 99;
        // Manually construct to override version
        let mut buf = BytesMut::with_capacity(32);
        buf.put_u32(MAGIC);
        buf.put_u8(99); // bad version
        buf.put_u8(0x01); // Data
        buf.put_u16(0); // flags
        buf.put_u16(1);
        buf.put_u16(2);
        buf.put_u32(0);
        buf.put_u64(0);
        buf.put_u32(0);
        buf.put_u32(0);
        assert!(decode_frame(&buf).is_err());
    }

    #[test]
    fn test_ack_payload_roundtrip() {
        let ack = AckPayload {
            ack_seq: 100,
            credit_grant: 5,
            reserved: 0,
            sack_bitmap: Some([0xAA; 32]),
        };
        let encoded = encode_ack_payload(&ack);
        let decoded = decode_ack_payload(&encoded, true).unwrap();
        assert_eq!(decoded.ack_seq, 100);
        assert_eq!(decoded.credit_grant, 5);
        assert_eq!(decoded.sack_bitmap.unwrap(), [0xAA; 32]);
    }

    #[test]
    fn test_ack_payload_no_sack() {
        let ack = AckPayload {
            ack_seq: 50,
            credit_grant: 3,
            reserved: 0,
            sack_bitmap: None,
        };
        let encoded = encode_ack_payload(&ack);
        let decoded = decode_ack_payload(&encoded, false).unwrap();
        assert_eq!(decoded.ack_seq, 50);
        assert!(decoded.sack_bitmap.is_none());
    }

    #[test]
    fn test_credit_payload_roundtrip() {
        let credit = CreditPayload {
            credits: 64,
            reserved: 0,
        };
        let encoded = encode_credit_payload(&credit);
        let decoded = decode_credit_payload(&encoded).unwrap();
        assert_eq!(decoded.credits, 64);
    }
}
