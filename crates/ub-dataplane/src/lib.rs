use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::{BufMut, BytesMut};
use dashmap::DashMap;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use ub_fabric::fabric::{Fabric, Session};
use ub_fabric::peer_addr::PeerAddr;
use ub_fabric::udp::UdpFabric;
use ub_wire::codec::{decode_frame, encode_frame};
use ub_wire::frame::*;

use ub_core::addr::UbAddr;
use ub_core::error::UbError;
use ub_core::jetty::JettyTable;
use ub_core::mr::{MrCacheTable, MrTable};
use ub_core::types::{JettyAddr, Verb};

/// Maximum payload size for M2 (no fragmentation).
pub const MAX_PAYLOAD_SIZE: usize = 1320; // 1400 PMTU - 80 bytes header

/// Default timeout for request-response verbs.
const VERB_TIMEOUT: Duration = Duration::from_secs(2);

/// Response from a remote verb operation.
#[derive(Debug)]
pub struct VerbResponse {
    pub status: u32,
    pub data: Vec<u8>,
}

/// Data plane engine — handles verb frame receive/dispatch and remote verb invocation.
pub struct DataPlaneEngine {
    local_node_id: u16,
    mr_table: Arc<MrTable>,
    mr_cache: Arc<MrCacheTable>,
    jetty_table: Arc<JettyTable>,
    fabric: Arc<UdpFabric>,
    pending_requests: Arc<DashMap<u64, oneshot::Sender<VerbResponse>>>,
    next_opaque: Arc<AtomicU64>,
    /// Outbound send channels to peer data plane addresses.
    peer_senders: Arc<DashMap<u16, mpsc::Sender<Vec<u8>>>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Clone for DataPlaneEngine {
    fn clone(&self) -> Self {
        DataPlaneEngine {
            local_node_id: self.local_node_id,
            mr_table: Arc::clone(&self.mr_table),
            mr_cache: Arc::clone(&self.mr_cache),
            jetty_table: Arc::clone(&self.jetty_table),
            fabric: Arc::clone(&self.fabric),
            pending_requests: Arc::clone(&self.pending_requests),
            next_opaque: Arc::clone(&self.next_opaque),
            peer_senders: Arc::clone(&self.peer_senders),
            tasks: Vec::new(), // tasks are owned by the original engine
        }
    }
}

impl DataPlaneEngine {
    pub fn new(
        local_node_id: u16,
        mr_table: Arc<MrTable>,
        mr_cache: Arc<MrCacheTable>,
        jetty_table: Arc<JettyTable>,
        fabric: Arc<UdpFabric>,
    ) -> Self {
        DataPlaneEngine {
            local_node_id,
            mr_table,
            mr_cache,
            jetty_table,
            fabric,
            pending_requests: Arc::new(DashMap::new()),
            next_opaque: Arc::new(AtomicU64::new(1)),
            peer_senders: Arc::new(DashMap::new()),
            tasks: Vec::new(),
        }
    }

    /// Start the data plane engine: listen for incoming sessions and process frames.
    pub async fn start(&mut self) -> Result<(), UbError> {
        let mut listener = self.fabric.listen(PeerAddr::Inet("0.0.0.0:0".parse().unwrap())).await?;

        let mr_table = Arc::clone(&self.mr_table);
        let mr_cache = Arc::clone(&self.mr_cache);
        let jetty_table = Arc::clone(&self.jetty_table);
        let pending = Arc::clone(&self.pending_requests);
        let local_node_id = self.local_node_id;
        let fabric = Arc::clone(&self.fabric);

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok(mut session) => {
                        let mr_table = Arc::clone(&mr_table);
                        let mr_cache = Arc::clone(&mr_cache);
                        let jetty_table = Arc::clone(&jetty_table);
                        let pending = Arc::clone(&pending);
                        let local_node_id = local_node_id;
                        let fabric = Arc::clone(&fabric);

                        tokio::spawn(async move {
                            loop {
                                match session.recv().await {
                                    Ok(raw_frame) => {
                                        handle_incoming_frame(
                                            &raw_frame, &mr_table, &mr_cache, &jetty_table, &pending,
                                            &mut session, local_node_id, &fabric,
                                        ).await;
                                    }
                                    Err(e) => {
                                        tracing::warn!("data plane recv error: {e}");
                                        break;
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("data plane accept error: {e}");
                    }
                }
            }
        });
        self.tasks.push(handle);

        tracing::info!("data plane engine started on {}", self.fabric.local_addr());
        Ok(())
    }

    /// Connect to a peer's data plane address.
    /// This registers the peer for outbound sends via fabric.send_to().
    /// Responses will arrive through the listener's accept() loop.
    pub async fn connect_peer(&self, node_id: u16, data_addr: std::net::SocketAddr) -> Result<(), UbError> {
        // Create a sender channel for this peer (for outbound writes)
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
        self.peer_senders.insert(node_id, tx);

        // Write loop: forward messages from channel to peer via fabric socket
        let fabric = Arc::clone(&self.fabric);
        let write_handle = tokio::spawn(async move {
            while let Some(data) = rx.recv().await {
                if let Err(e) = fabric.send_to(data_addr, &data).await {
                    tracing::warn!("data plane send error: {e}");
                    break;
                }
            }
        });

        let _ = write_handle;
        tracing::info!("data plane: connected to peer {} at {}", node_id, data_addr);
        Ok(())
    }

    /// Remote write (fire-and-forget).
    pub async fn ub_write_remote(
        &self,
        addr: UbAddr,
        data: &[u8],
    ) -> Result<(), UbError> {
        if data.len() > MAX_PAYLOAD_SIZE {
            return Err(UbError::PayloadTooLarge);
        }

        let cache_entry = self.mr_cache.lookup_by_addr(addr)
            .ok_or(UbError::AddrInvalid)?;

        let sender = self.peer_senders.get(&cache_entry.owner_node)
            .ok_or(UbError::LinkDown)?;

        let frame = build_data_frame(
            self.local_node_id,
            cache_entry.owner_node,
            Verb::Write,
            cache_entry.remote_mr_handle,
            addr,
            0, // opaque — no response expected
            data,
        );

        sender.send(frame.to_vec()).await
            .map_err(|_| UbError::LinkDown)?;
        Ok(())
    }

    /// Remote read (request-response).
    pub async fn ub_read_remote(
        &self,
        addr: UbAddr,
        len: u32,
    ) -> Result<Vec<u8>, UbError> {
        let cache_entry = self.mr_cache.lookup_by_addr(addr)
            .ok_or(UbError::AddrInvalid)?;

        let sender = self.peer_senders.get(&cache_entry.owner_node)
            .ok_or(UbError::LinkDown)?;

        let opaque = self.next_opaque.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending_requests.insert(opaque, tx);

        let mut payload = BytesMut::with_capacity(4);
        payload.put_u32(len);

        let frame = build_data_frame(
            self.local_node_id,
            cache_entry.owner_node,
            Verb::ReadReq,
            cache_entry.remote_mr_handle,
            addr,
            opaque,
            &payload,
        );

        sender.send(frame.to_vec()).await
            .map_err(|_| UbError::LinkDown)?;
        drop(sender); // Release the DashMap reference

        match tokio::time::timeout(VERB_TIMEOUT, rx).await {
            Ok(Ok(resp)) => {
                if resp.status != 0 {
                    return Err(status_to_error(resp.status));
                }
                Ok(resp.data)
            }
            _ => {
                self.pending_requests.remove(&opaque);
                Err(UbError::Timeout)
            }
        }
    }

    /// Remote atomic CAS (request-response).
    pub async fn ub_atomic_cas_remote(
        &self,
        addr: UbAddr,
        expect: u64,
        new: u64,
    ) -> Result<u64, UbError> {
        let cache_entry = self.mr_cache.lookup_by_addr(addr)
            .ok_or(UbError::AddrInvalid)?;

        let sender = self.peer_senders.get(&cache_entry.owner_node)
            .ok_or(UbError::LinkDown)?;

        let opaque = self.next_opaque.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending_requests.insert(opaque, tx);

        let mut payload = BytesMut::with_capacity(16);
        payload.put_u64(expect);
        payload.put_u64(new);

        let frame = build_data_frame(
            self.local_node_id,
            cache_entry.owner_node,
            Verb::AtomicCas,
            cache_entry.remote_mr_handle,
            addr,
            opaque,
            &payload,
        );

        sender.send(frame.to_vec()).await
            .map_err(|_| UbError::LinkDown)?;
        drop(sender);

        match tokio::time::timeout(VERB_TIMEOUT, rx).await {
            Ok(Ok(resp)) => {
                if resp.status != 0 {
                    return Err(status_to_error(resp.status));
                }
                if resp.data.len() >= 8 {
                    let old_value = u64::from_be_bytes(resp.data[..8].try_into().unwrap());
                    Ok(old_value)
                } else {
                    Err(UbError::Internal("atomic CAS response too short".into()))
                }
            }
            _ => {
                self.pending_requests.remove(&opaque);
                Err(UbError::Timeout)
            }
        }
    }

    /// Remote atomic FAA (request-response).
    pub async fn ub_atomic_faa_remote(
        &self,
        addr: UbAddr,
        add: u64,
    ) -> Result<u64, UbError> {
        let cache_entry = self.mr_cache.lookup_by_addr(addr)
            .ok_or(UbError::AddrInvalid)?;

        let sender = self.peer_senders.get(&cache_entry.owner_node)
            .ok_or(UbError::LinkDown)?;

        let opaque = self.next_opaque.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending_requests.insert(opaque, tx);

        let mut payload = BytesMut::with_capacity(8);
        payload.put_u64(add);

        let frame = build_data_frame(
            self.local_node_id,
            cache_entry.owner_node,
            Verb::AtomicFaa,
            cache_entry.remote_mr_handle,
            addr,
            opaque,
            &payload,
        );

        sender.send(frame.to_vec()).await
            .map_err(|_| UbError::LinkDown)?;
        drop(sender);

        match tokio::time::timeout(VERB_TIMEOUT, rx).await {
            Ok(Ok(resp)) => {
                if resp.status != 0 {
                    return Err(status_to_error(resp.status));
                }
                if resp.data.len() >= 8 {
                    let old_value = u64::from_be_bytes(resp.data[..8].try_into().unwrap());
                    Ok(old_value)
                } else {
                    Err(UbError::Internal("atomic FAA response too short".into()))
                }
            }
            _ => {
                self.pending_requests.remove(&opaque);
                Err(UbError::Timeout)
            }
        }
    }

    /// Remote send (fire-and-forget, bilateral message).
    pub async fn ub_send_remote(
        &self,
        jetty_src: u32,
        dst_jetty: JettyAddr,
        data: &[u8],
    ) -> Result<(), UbError> {
        if data.len() > MAX_PAYLOAD_SIZE {
            return Err(UbError::PayloadTooLarge);
        }

        let sender = self.peer_senders.get(&dst_jetty.node_id)
            .ok_or(UbError::LinkDown)?;

        let frame = build_data_frame_ex(
            self.local_node_id,
            dst_jetty.node_id,
            Verb::Send,
            0, // no MR handle
            UbAddr::new(0, 0, 0, 0, 0), // no UB address
            0, // no opaque
            jetty_src,
            dst_jetty.jetty_id,
            None, // no imm
            data,
        );

        sender.send(frame.to_vec()).await
            .map_err(|_| UbError::LinkDown)?;
        Ok(())
    }

    /// Remote send with immediate (fire-and-forget, bilateral message + imm).
    pub async fn ub_send_with_imm_remote(
        &self,
        jetty_src: u32,
        dst_jetty: JettyAddr,
        data: &[u8],
        imm: u64,
    ) -> Result<(), UbError> {
        if data.len() > MAX_PAYLOAD_SIZE {
            return Err(UbError::PayloadTooLarge);
        }

        let sender = self.peer_senders.get(&dst_jetty.node_id)
            .ok_or(UbError::LinkDown)?;

        let frame = build_data_frame_ex(
            self.local_node_id,
            dst_jetty.node_id,
            Verb::Send,
            0,
            UbAddr::new(0, 0, 0, 0, 0),
            0,
            jetty_src,
            dst_jetty.jetty_id,
            Some(imm),
            data,
        );

        sender.send(frame.to_vec()).await
            .map_err(|_| UbError::LinkDown)?;
        Ok(())
    }

    /// Remote write with immediate (fire-and-forget, unilateral write + JFC CQE notification).
    pub async fn ub_write_imm_remote(
        &self,
        addr: UbAddr,
        data: &[u8],
        imm: u64,
        jetty_src: u32,
        dst_jetty: JettyAddr,
    ) -> Result<(), UbError> {
        if data.len() > MAX_PAYLOAD_SIZE {
            return Err(UbError::PayloadTooLarge);
        }

        let cache_entry = self.mr_cache.lookup_by_addr(addr)
            .ok_or(UbError::AddrInvalid)?;

        let sender = self.peer_senders.get(&cache_entry.owner_node)
            .ok_or(UbError::LinkDown)?;

        let frame = build_data_frame_ex(
            self.local_node_id,
            cache_entry.owner_node,
            Verb::WriteImm,
            cache_entry.remote_mr_handle,
            addr,
            0,
            jetty_src,
            dst_jetty.jetty_id,
            Some(imm),
            data,
        );

        sender.send(frame.to_vec()).await
            .map_err(|_| UbError::LinkDown)?;
        Ok(())
    }

    pub fn fabric(&self) -> &Arc<UdpFabric> {
        &self.fabric
    }

    pub fn jetty_table(&self) -> &Arc<JettyTable> {
        &self.jetty_table
    }
}

/// Handle an incoming frame on a listener session.
async fn handle_incoming_frame(
    raw: &[u8],
    mr_table: &MrTable,
    _mr_cache: &MrCacheTable,
    jetty_table: &JettyTable,
    pending: &DashMap<u64, oneshot::Sender<VerbResponse>>,
    session: &mut Box<dyn Session>,
    local_node_id: u16,
    _fabric: &Arc<UdpFabric>,
) {
    if let Ok((header, ext, payload)) = decode_frame(raw) {
        if let Some(ref ext_header) = ext {
            match ext_header.verb {
                Verb::Write => {
                    handle_write(payload, ext_header, mr_table);
                }
                Verb::ReadReq => {
                    handle_read_req(payload, ext_header, mr_table, session, local_node_id, &header).await;
                }
                Verb::AtomicCas => {
                    handle_atomic_cas(payload, ext_header, mr_table, session, local_node_id, &header).await;
                }
                Verb::AtomicFaa => {
                    handle_atomic_faa(payload, ext_header, mr_table, session, local_node_id, &header).await;
                }
                Verb::Send => {
                    handle_send(payload, ext_header, jetty_table);
                }
                Verb::WriteImm => {
                    handle_write_imm(payload, ext_header, mr_table, jetty_table);
                }
                // Response verbs — complete pending oneshot
                Verb::ReadResp | Verb::AtomicCasResp | Verb::AtomicFaaResp => {
                    complete_pending_request(ext_header, payload, pending);
                }
            }
        }
    }
}

fn handle_write(
    payload: &[u8],
    ext: &DataExtHeader,
    mr_table: &MrTable,
) {
    let entry = match mr_table.lookup(ext.mr_handle) {
        Some(e) => e,
        None => {
            tracing::warn!("WRITE: MR handle {} not found", ext.mr_handle);
            return;
        }
    };

    if let Err(e) = entry.check_perms(Verb::Write) {
        tracing::warn!("WRITE: permission denied for MR {}: {e}", ext.mr_handle);
        return;
    }

    let offset_in_mr = if ext.ub_addr.offset() >= entry.base_ub_addr.offset() {
        ext.ub_addr.offset() - entry.base_ub_addr.offset()
    } else {
        0
    };
    let device_offset = entry.base_offset + offset_in_mr;

    if let Err(e) = entry.device.write(device_offset, payload) {
        tracing::warn!("WRITE: device write error: {e}");
    }
}

async fn handle_read_req(
    payload: &[u8],
    ext: &DataExtHeader,
    mr_table: &MrTable,
    session: &mut Box<dyn Session>,
    local_node_id: u16,
    header: &FrameHeader,
) {
    let entry = match mr_table.lookup(ext.mr_handle) {
        Some(e) => e,
        None => {
            send_error_response(session, local_node_id, header.src_node, ext, 1).await;
            return;
        }
    };

    if entry.check_perms(Verb::ReadReq).is_err() {
        send_error_response(session, local_node_id, header.src_node, ext, 2).await;
        return;
    }

    let read_len = if payload.len() >= 4 {
        u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize
    } else {
        1024
    };

    let offset_in_mr = if ext.ub_addr.offset() >= entry.base_ub_addr.offset() {
        ext.ub_addr.offset() - entry.base_ub_addr.offset()
    } else {
        0
    };
    let device_offset = entry.base_offset + offset_in_mr;

    let mut buf = vec![0u8; read_len];
    match entry.device.read(device_offset, &mut buf) {
        Ok(()) => {
            let resp_frame = build_data_frame(
                local_node_id,
                header.src_node,
                Verb::ReadResp,
                ext.mr_handle,
                ext.ub_addr,
                ext.opaque,
                &buf,
            );
            let _ = session.send(&resp_frame).await;
        }
        Err(_) => {
            send_error_response(session, local_node_id, header.src_node, ext, 1).await;
        }
    }
}

async fn handle_atomic_cas(
    payload: &[u8],
    ext: &DataExtHeader,
    mr_table: &MrTable,
    session: &mut Box<dyn Session>,
    local_node_id: u16,
    header: &FrameHeader,
) {
    let entry = match mr_table.lookup(ext.mr_handle) {
        Some(e) => e,
        None => {
            send_error_response(session, local_node_id, header.src_node, ext, 1).await;
            return;
        }
    };

    if entry.check_perms(Verb::AtomicCas).is_err() {
        send_error_response(session, local_node_id, header.src_node, ext, 2).await;
        return;
    }

    let offset_in_mr = if ext.ub_addr.offset() >= entry.base_ub_addr.offset() {
        ext.ub_addr.offset() - entry.base_ub_addr.offset()
    } else {
        0
    };
    if offset_in_mr % 8 != 0 {
        send_error_response(session, local_node_id, header.src_node, ext, 3).await;
        return;
    }

    let device_offset = entry.base_offset + offset_in_mr;

    if payload.len() < 16 {
        send_error_response(session, local_node_id, header.src_node, ext, 9).await;
        return;
    }
    let expect = u64::from_be_bytes(payload[..8].try_into().unwrap());
    let new = u64::from_be_bytes(payload[8..16].try_into().unwrap());

    match entry.device.atomic_cas(device_offset, expect, new) {
        Ok(old_value) => {
            let mut resp_payload = BytesMut::with_capacity(16);
            resp_payload.put_u64(old_value);
            resp_payload.put_u32(0);
            resp_payload.put_u32(0);

            let resp_frame = build_data_frame(
                local_node_id,
                header.src_node,
                Verb::AtomicCasResp,
                ext.mr_handle,
                ext.ub_addr,
                ext.opaque,
                &resp_payload,
            );
            let _ = session.send(&resp_frame).await;
        }
        Err(_) => {
            send_error_response(session, local_node_id, header.src_node, ext, 9).await;
        }
    }
}

async fn handle_atomic_faa(
    payload: &[u8],
    ext: &DataExtHeader,
    mr_table: &MrTable,
    session: &mut Box<dyn Session>,
    local_node_id: u16,
    header: &FrameHeader,
) {
    let entry = match mr_table.lookup(ext.mr_handle) {
        Some(e) => e,
        None => {
            send_error_response(session, local_node_id, header.src_node, ext, 1).await;
            return;
        }
    };

    if entry.check_perms(Verb::AtomicFaa).is_err() {
        send_error_response(session, local_node_id, header.src_node, ext, 2).await;
        return;
    }

    let offset_in_mr = if ext.ub_addr.offset() >= entry.base_ub_addr.offset() {
        ext.ub_addr.offset() - entry.base_ub_addr.offset()
    } else {
        0
    };
    if offset_in_mr % 8 != 0 {
        send_error_response(session, local_node_id, header.src_node, ext, 3).await;
        return;
    }

    let device_offset = entry.base_offset + offset_in_mr;

    if payload.len() < 8 {
        send_error_response(session, local_node_id, header.src_node, ext, 9).await;
        return;
    }
    let add = u64::from_be_bytes(payload[..8].try_into().unwrap());

    match entry.device.atomic_faa(device_offset, add) {
        Ok(old_value) => {
            let mut resp_payload = BytesMut::with_capacity(16);
            resp_payload.put_u64(old_value);
            resp_payload.put_u32(0);
            resp_payload.put_u32(0);

            let resp_frame = build_data_frame(
                local_node_id,
                header.src_node,
                Verb::AtomicFaaResp,
                ext.mr_handle,
                ext.ub_addr,
                ext.opaque,
                &resp_payload,
            );
            let _ = session.send(&resp_frame).await;
        }
        Err(_) => {
            send_error_response(session, local_node_id, header.src_node, ext, 9).await;
        }
    }
}

fn complete_pending_request(
    ext: &DataExtHeader,
    payload: &[u8],
    pending: &DashMap<u64, oneshot::Sender<VerbResponse>>,
) {
    if let Some((_, sender)) = pending.remove(&ext.opaque) {
        let is_error = ext.ext_flags.contains(ExtFlags::ERR_RESP);
        let status = if is_error && payload.len() >= 4 {
            u32::from_be_bytes(payload[..4].try_into().unwrap())
        } else {
            0
        };

        let data = if is_error { Vec::new() } else { payload.to_vec() };

        let _ = sender.send(VerbResponse { status, data });
    }
}

/// Handle incoming Send frame — match to a posted recv buffer and generate CQE.
fn handle_send(
    payload: &[u8],
    ext: &DataExtHeader,
    jetty_table: &JettyTable,
) {
    let jetty = match jetty_table.lookup(ext.jetty_dst) {
        Some(j) => j,
        None => {
            tracing::warn!("SEND: jetty {} not found", ext.jetty_dst);
            return;
        }
    };

    // Pop a pre-posted receive buffer from JFR
    let mut recv_buf = match jetty.pop_recv() {
        Some(buf) => buf,
        None => {
            tracing::warn!("SEND: no recv buffer posted on jetty {}, dropping", ext.jetty_dst);
            return;
        }
    };

    // Copy payload into recv buffer (truncate if needed)
    let copy_len = std::cmp::min(payload.len(), recv_buf.buf.len());
    recv_buf.buf[..copy_len].copy_from_slice(&payload[..copy_len]);

    // Push CQE onto JFC
    let cqe = ub_core::types::Cqe {
        wr_id: recv_buf.wr_id,
        status: ub_core::error::UB_OK,
        imm: ext.imm,
        byte_len: copy_len as u32,
        jetty_id: ext.jetty_dst,
        verb: Verb::Send,
    };
    if let Err(e) = jetty.push_cqe(cqe) {
        tracing::warn!("SEND: failed to push CQE to jetty {}: {e}", ext.jetty_dst);
    }
}

/// Handle incoming WriteImm frame — write data to MR and generate CQE on target Jetty.
fn handle_write_imm(
    payload: &[u8],
    ext: &DataExtHeader,
    mr_table: &MrTable,
    jetty_table: &JettyTable,
) {
    // Write payload to MR (same as handle_write)
    let entry = match mr_table.lookup(ext.mr_handle) {
        Some(e) => e,
        None => {
            tracing::warn!("WRITE_IMM: MR handle {} not found", ext.mr_handle);
            return;
        }
    };

    if let Err(e) = entry.check_perms(Verb::WriteImm) {
        tracing::warn!("WRITE_IMM: permission denied for MR {}: {e}", ext.mr_handle);
        return;
    }

    let offset_in_mr = if ext.ub_addr.offset() >= entry.base_ub_addr.offset() {
        ext.ub_addr.offset() - entry.base_ub_addr.offset()
    } else {
        0
    };
    let device_offset = entry.base_offset + offset_in_mr;

    if let Err(e) = entry.device.write(device_offset, payload) {
        tracing::warn!("WRITE_IMM: device write error: {e}");
        return;
    }

    // Generate CQE on target Jetty
    let jetty = match jetty_table.lookup(ext.jetty_dst) {
        Some(j) => j,
        None => {
            tracing::warn!("WRITE_IMM: jetty {} not found, write completed but no CQE", ext.jetty_dst);
            return;
        }
    };

    let cqe = ub_core::types::Cqe {
        wr_id: 0,
        status: ub_core::error::UB_OK,
        imm: ext.imm,
        byte_len: payload.len() as u32,
        jetty_id: ext.jetty_dst,
        verb: Verb::WriteImm,
    };
    if let Err(e) = jetty.push_cqe(cqe) {
        tracing::warn!("WRITE_IMM: failed to push CQE to jetty {}: {e}", ext.jetty_dst);
    }
}

async fn send_error_response(
    session: &mut Box<dyn Session>,
    local_node_id: u16,
    dst_node: u16,
    req_ext: &DataExtHeader,
    status: u32,
) {
    let mut resp_payload = BytesMut::with_capacity(4);
    resp_payload.put_u32(status);

    let resp_frame = build_data_frame_with_flags(
        local_node_id,
        dst_node,
        req_ext.verb,
        req_ext.mr_handle,
        req_ext.ub_addr,
        req_ext.opaque,
        &resp_payload,
        ExtFlags::ERR_RESP,
    );

    let _ = session.send(&resp_frame).await;
}

fn build_data_frame(
    src_node: u16,
    dst_node: u16,
    verb: Verb,
    mr_handle: u32,
    ub_addr: UbAddr,
    opaque: u64,
    payload: &[u8],
) -> BytesMut {
    build_data_frame_with_flags(src_node, dst_node, verb, mr_handle, ub_addr, opaque, payload, ExtFlags::empty())
}

fn build_data_frame_with_flags(
    src_node: u16,
    dst_node: u16,
    verb: Verb,
    mr_handle: u32,
    ub_addr: UbAddr,
    opaque: u64,
    payload: &[u8],
    ext_flags: ExtFlags,
) -> BytesMut {
    let header = FrameHeader {
        magic: MAGIC,
        version: VERSION,
        frame_type: FrameType::Data,
        flags: FrameFlags::empty(),
        src_node,
        dst_node,
        reserved: 0,
        stream_seq: 0,
        payload_len: payload.len() as u32,
        header_crc: 0,
    };

    let ext = DataExtHeader {
        verb,
        ext_flags,
        mr_handle,
        jetty_src: 0,
        jetty_dst: 0,
        opaque,
        frag_id: 0,
        frag_index: 0,
        frag_total: 1,
        ub_addr,
        imm: None,
    };

    encode_frame(&header, Some(&ext), payload)
}

/// Extended frame builder with Jetty and immediate value support.
fn build_data_frame_ex(
    src_node: u16,
    dst_node: u16,
    verb: Verb,
    mr_handle: u32,
    ub_addr: UbAddr,
    opaque: u64,
    jetty_src: u32,
    jetty_dst: u32,
    imm: Option<u64>,
    payload: &[u8],
) -> BytesMut {
    let mut ext_flags = ExtFlags::empty();
    let mut frame_flags = FrameFlags::empty();
    if imm.is_some() {
        ext_flags |= ExtFlags::HAS_IMM;
        frame_flags |= FrameFlags::HAS_IMM;
    }

    let header = FrameHeader {
        magic: MAGIC,
        version: VERSION,
        frame_type: FrameType::Data,
        flags: frame_flags,
        src_node,
        dst_node,
        reserved: 0,
        stream_seq: 0,
        payload_len: payload.len() as u32,
        header_crc: 0,
    };

    let ext = DataExtHeader {
        verb,
        ext_flags,
        mr_handle,
        jetty_src,
        jetty_dst,
        opaque,
        frag_id: 0,
        frag_index: 0,
        frag_total: 1,
        ub_addr,
        imm,
    };

    encode_frame(&header, Some(&ext), payload)
}

fn status_to_error(status: u32) -> UbError {
    match status {
        1 => UbError::AddrInvalid,
        2 => UbError::PermDenied,
        3 => UbError::Alignment,
        4 => UbError::LinkDown,
        5 => UbError::NoResources,
        6 => UbError::Timeout,
        7 => UbError::PayloadTooLarge,
        8 => UbError::Flushed,
        _ => UbError::Internal(format!("unknown status: {status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ub_core::config::JettyConfig;
    use ub_core::device::memory::MemoryDevice;
    use ub_core::jetty::JettyTable;
    use ub_core::types::{DeviceKind, MrPerms};

    fn test_jetty_config() -> JettyConfig {
        JettyConfig {
            jfs_depth: 16,
            jfr_depth: 16,
            jfc_depth: 16,
            jfc_high_watermark: 12,
        }
    }

    #[test]
    fn test_build_data_frame() {
        let frame = build_data_frame(
            1, 2, Verb::Write, 5, UbAddr::new(1, 2, 0, 0, 0), 42, &[1, 2, 3, 4],
        );
        let (header, ext, payload) = decode_frame(&frame).unwrap();
        assert_eq!(header.src_node, 1);
        assert_eq!(header.dst_node, 2);
        let ext = ext.unwrap();
        assert_eq!(ext.verb, Verb::Write);
        assert_eq!(ext.mr_handle, 5);
        assert_eq!(ext.opaque, 42);
        assert_eq!(payload, &[1, 2, 3, 4]);
    }

    #[test]
    fn test_status_to_error() {
        assert!(matches!(status_to_error(1), UbError::AddrInvalid));
        assert!(matches!(status_to_error(2), UbError::PermDenied));
        assert!(matches!(status_to_error(3), UbError::Alignment));
    }

    #[tokio::test]
    async fn test_dataplane_write_and_read() {
        let fabric_a = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());
        let fabric_b = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());

        let mr_table_a = Arc::new(MrTable::new(1, 1));
        let mr_cache_a = Arc::new(MrCacheTable::new());
        let mr_table_b = Arc::new(MrTable::new(1, 2));
        let mr_cache_b = Arc::new(MrCacheTable::new());

        let dev = Arc::new(MemoryDevice::new(4096));
        let (addr_a, handle_a) = mr_table_a.register(dev, 1024, MrPerms::READ | MrPerms::WRITE | MrPerms::ATOMIC).unwrap();

        mr_cache_b.insert(ub_core::mr::MrCacheEntry {
            remote_mr_handle: handle_a.0,
            owner_node: 1,
            base_ub_addr: addr_a,
            len: 1024,
            perms: MrPerms::READ | MrPerms::WRITE | MrPerms::ATOMIC,
            device_kind: DeviceKind::Memory,
        });

        let mut engine_a = DataPlaneEngine::new(1, Arc::clone(&mr_table_a), mr_cache_a, Arc::new(JettyTable::new(1, test_jetty_config())), fabric_a);
        let mut engine_b = DataPlaneEngine::new(2, mr_table_b, mr_cache_b, Arc::new(JettyTable::new(2, test_jetty_config())), fabric_b);

        engine_a.start().await.unwrap();
        engine_b.start().await.unwrap();

        let addr_a_dp = engine_a.fabric.local_addr();
        engine_b.connect_peer(1, addr_a_dp).await.unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        let write_data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        engine_b.ub_write_remote(addr_a, &write_data).await.unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;

        let entry = mr_table_a.lookup(handle_a.0).unwrap();
        let mut read_buf = vec![0u8; 4];
        entry.device.read(entry.base_offset, &mut read_buf).unwrap();
        assert_eq!(&read_buf, &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    /// Cross-node atomic CAS/FAA integration test.
    #[tokio::test]
    async fn test_dataplane_atomic_cas_and_faa() {
        let fabric_a = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());
        let fabric_b = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());

        let mr_table_a = Arc::new(MrTable::new(1, 1));
        let mr_cache_a = Arc::new(MrCacheTable::new());
        let mr_table_b = Arc::new(MrTable::new(1, 2));
        let mr_cache_b = Arc::new(MrCacheTable::new());

        let dev = Arc::new(MemoryDevice::new(4096));
        let (addr_a, handle_a) = mr_table_a.register(dev, 4096, MrPerms::READ | MrPerms::WRITE | MrPerms::ATOMIC).unwrap();

        mr_cache_b.insert(ub_core::mr::MrCacheEntry {
            remote_mr_handle: handle_a.0,
            owner_node: 1,
            base_ub_addr: addr_a,
            len: 4096,
            perms: MrPerms::READ | MrPerms::WRITE | MrPerms::ATOMIC,
            device_kind: DeviceKind::Memory,
        });

        let mut engine_a = DataPlaneEngine::new(1, Arc::clone(&mr_table_a), mr_cache_a, Arc::new(JettyTable::new(1, test_jetty_config())), fabric_a);
        let mut engine_b = DataPlaneEngine::new(2, mr_table_b, mr_cache_b, Arc::new(JettyTable::new(2, test_jetty_config())), fabric_b);

        engine_a.start().await.unwrap();
        engine_b.start().await.unwrap();

        let addr_a_dp = engine_a.fabric.local_addr();
        engine_b.connect_peer(1, addr_a_dp).await.unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Initialize to 0
        engine_b.ub_write_remote(addr_a, &0u64.to_ne_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // CAS: expect 0, new 42 → should succeed, old=0
        let old = engine_b.ub_atomic_cas_remote(addr_a, 0, 42).await.unwrap();
        assert_eq!(old, 0, "CAS should return old_value=0");

        // CAS: expect 0, new 99 → should fail, old=42
        let old = engine_b.ub_atomic_cas_remote(addr_a, 0, 99).await.unwrap();
        assert_eq!(old, 42, "CAS retry should return old_value=42");

        // FAA: add 1 → returns 42, value becomes 43
        let old = engine_b.ub_atomic_faa_remote(addr_a, 1).await.unwrap();
        assert_eq!(old, 42, "FAA should return old_value=42");
    }

    /// Concurrent CAS serialization test: 8 tasks race to CAS from 0 to their task ID.
    /// Exactly one should succeed (return old=0).
    #[tokio::test]
    async fn test_concurrent_cas_serialization() {
        let fabric_a = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());
        let fabric_b = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());

        let mr_table_a = Arc::new(MrTable::new(1, 1));
        let mr_cache_a = Arc::new(MrCacheTable::new());
        let mr_table_b = Arc::new(MrTable::new(1, 2));
        let mr_cache_b = Arc::new(MrCacheTable::new());

        let dev = Arc::new(MemoryDevice::new(4096));
        let (addr_a, handle_a) = mr_table_a.register(dev, 4096, MrPerms::READ | MrPerms::WRITE | MrPerms::ATOMIC).unwrap();

        mr_cache_b.insert(ub_core::mr::MrCacheEntry {
            remote_mr_handle: handle_a.0,
            owner_node: 1,
            base_ub_addr: addr_a,
            len: 4096,
            perms: MrPerms::READ | MrPerms::WRITE | MrPerms::ATOMIC,
            device_kind: DeviceKind::Memory,
        });

        let mut engine_a = DataPlaneEngine::new(1, Arc::clone(&mr_table_a), mr_cache_a, Arc::new(JettyTable::new(1, test_jetty_config())), fabric_a);
        let mut engine_b = DataPlaneEngine::new(2, mr_table_b, mr_cache_b, Arc::new(JettyTable::new(2, test_jetty_config())), fabric_b);

        engine_a.start().await.unwrap();
        engine_b.start().await.unwrap();

        let addr_a_dp = engine_a.fabric.local_addr();
        engine_b.connect_peer(1, addr_a_dp).await.unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Initialize to 0
        engine_b.ub_write_remote(addr_a, &0u64.to_ne_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Spawn 8 concurrent CAS tasks
        let mut handles = Vec::new();
        for task_id in 1u64..=8 {
            let engine = engine_b.clone();
            handles.push(tokio::spawn(async move {
                engine.ub_atomic_cas_remote(addr_a, 0, task_id).await
            }));
        }

        // Collect results
        let mut success_count = 0;
        let mut winner_id = 0u64;
        for handle in handles {
            match handle.await.unwrap() {
                Ok(old) => {
                    if old == 0 {
                        success_count += 1;
                    }
                }
                Err(e) => {
                    // Timeout or other error is acceptable in concurrent scenario
                    eprintln!("CAS error: {e}");
                }
            }
        }

        // Verify the final value is one of the task IDs
        let read_data = engine_b.ub_read_remote(addr_a, 8).await.unwrap();
        let final_value = u64::from_ne_bytes(read_data[..8].try_into().unwrap());
        assert!(
            (1..=8).contains(&final_value),
            "Final value should be a task ID, got {final_value}"
        );
        winner_id = final_value;

        // At least one CAS should succeed (exactly one if serialization works correctly)
        assert!(
            success_count >= 1,
            "At least one CAS should succeed (got {success_count})"
        );
        assert!(
            success_count <= 1,
            "At most one CAS should succeed with old=0 (got {success_count}, winner={winner_id})"
        );
    }

    #[test]
    fn test_build_data_frame_ex_with_imm() {
        let frame = build_data_frame_ex(
            1, 2, Verb::Send, 0, UbAddr::new(0, 0, 0, 0, 0), 0,
            10, 20, Some(42), &[1, 2, 3],
        );
        let (header, ext, payload) = decode_frame(&frame).unwrap();
        assert_eq!(header.src_node, 1);
        assert!(header.flags.contains(FrameFlags::HAS_IMM));
        let ext = ext.unwrap();
        assert_eq!(ext.verb, Verb::Send);
        assert!(ext.ext_flags.contains(ExtFlags::HAS_IMM));
        assert_eq!(ext.jetty_src, 10);
        assert_eq!(ext.jetty_dst, 20);
        assert_eq!(ext.imm, Some(42));
        assert_eq!(payload, &[1, 2, 3]);
    }

    #[tokio::test]
    async fn test_dataplane_send_and_recv() {
        let fabric_a = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());
        let fabric_b = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());

        let mr_table_a = Arc::new(MrTable::new(1, 1));
        let mr_cache_a = Arc::new(MrCacheTable::new());
        let mr_table_b = Arc::new(MrTable::new(1, 2));
        let mr_cache_b = Arc::new(MrCacheTable::new());

        let jetty_table_a = Arc::new(JettyTable::new(1, test_jetty_config()));
        let jetty_table_b = Arc::new(JettyTable::new(2, test_jetty_config()));

        let mut engine_a = DataPlaneEngine::new(1, Arc::clone(&mr_table_a), mr_cache_a, Arc::clone(&jetty_table_a), fabric_a);
        let mut engine_b = DataPlaneEngine::new(2, mr_table_b, mr_cache_b, Arc::clone(&jetty_table_b), fabric_b);

        engine_a.start().await.unwrap();
        engine_b.start().await.unwrap();

        let addr_a_dp = engine_a.fabric.local_addr();
        engine_b.connect_peer(1, addr_a_dp).await.unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Create Jetty on node A
        let jetty_handle_a = jetty_table_a.create().unwrap();
        let jetty_a = jetty_table_a.lookup(jetty_handle_a.0).unwrap();

        // Post recv buffer on node A's Jetty
        jetty_a.post_recv(vec![0u8; 256], 100).unwrap();

        // Send from node B to node A's Jetty
        let dst_jetty = JettyAddr { node_id: 1, jetty_id: jetty_handle_a.0 };
        engine_b.ub_send_remote(1, dst_jetty, &[72, 101, 108, 108, 111]).await.unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Poll CQE on node A
        let cqe = jetty_a.poll_cqe();
        assert!(cqe.is_some(), "Should have a CQE from the Send");
        let cqe = cqe.unwrap();
        assert_eq!(cqe.wr_id, 100);
        assert_eq!(cqe.byte_len, 5);
        assert_eq!(cqe.verb, Verb::Send);
        assert_eq!(cqe.imm, None);
    }

    #[tokio::test]
    async fn test_dataplane_send_with_imm() {
        let fabric_a = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());
        let fabric_b = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());

        let mr_table_a = Arc::new(MrTable::new(1, 1));
        let mr_cache_a = Arc::new(MrCacheTable::new());
        let mr_table_b = Arc::new(MrTable::new(1, 2));
        let mr_cache_b = Arc::new(MrCacheTable::new());

        let jetty_table_a = Arc::new(JettyTable::new(1, test_jetty_config()));
        let jetty_table_b = Arc::new(JettyTable::new(2, test_jetty_config()));

        let mut engine_a = DataPlaneEngine::new(1, Arc::clone(&mr_table_a), mr_cache_a, Arc::clone(&jetty_table_a), fabric_a);
        let mut engine_b = DataPlaneEngine::new(2, mr_table_b, mr_cache_b, Arc::clone(&jetty_table_b), fabric_b);

        engine_a.start().await.unwrap();
        engine_b.start().await.unwrap();

        let addr_a_dp = engine_a.fabric.local_addr();
        engine_b.connect_peer(1, addr_a_dp).await.unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        let jetty_handle_a = jetty_table_a.create().unwrap();
        let jetty_a = jetty_table_a.lookup(jetty_handle_a.0).unwrap();
        jetty_a.post_recv(vec![0u8; 256], 200).unwrap();

        let dst_jetty = JettyAddr { node_id: 1, jetty_id: jetty_handle_a.0 };
        engine_b.ub_send_with_imm_remote(1, dst_jetty, &[1, 2, 3], 42).await.unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;

        let cqe = jetty_a.poll_cqe().unwrap();
        assert_eq!(cqe.wr_id, 200);
        assert_eq!(cqe.imm, Some(42));
        assert_eq!(cqe.byte_len, 3);
    }

    #[tokio::test]
    async fn test_dataplane_write_imm() {
        let fabric_a = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());
        let fabric_b = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());

        let mr_table_a = Arc::new(MrTable::new(1, 1));
        let mr_cache_a = Arc::new(MrCacheTable::new());
        let mr_table_b = Arc::new(MrTable::new(1, 2));
        let mr_cache_b = Arc::new(MrCacheTable::new());

        let dev = Arc::new(MemoryDevice::new(4096));
        let (addr_a, handle_a) = mr_table_a.register(dev, 4096, MrPerms::READ | MrPerms::WRITE).unwrap();

        mr_cache_b.insert(ub_core::mr::MrCacheEntry {
            remote_mr_handle: handle_a.0,
            owner_node: 1,
            base_ub_addr: addr_a,
            len: 4096,
            perms: MrPerms::READ | MrPerms::WRITE,
            device_kind: DeviceKind::Memory,
        });

        let jetty_table_a = Arc::new(JettyTable::new(1, test_jetty_config()));
        let jetty_table_b = Arc::new(JettyTable::new(2, test_jetty_config()));

        let mut engine_a = DataPlaneEngine::new(1, Arc::clone(&mr_table_a), mr_cache_a, Arc::clone(&jetty_table_a), fabric_a);
        let mut engine_b = DataPlaneEngine::new(2, mr_table_b, mr_cache_b, Arc::clone(&jetty_table_b), fabric_b);

        engine_a.start().await.unwrap();
        engine_b.start().await.unwrap();

        let addr_a_dp = engine_a.fabric.local_addr();
        engine_b.connect_peer(1, addr_a_dp).await.unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Create Jetty on node A for CQE notification
        let jetty_handle_a = jetty_table_a.create().unwrap();
        let jetty_a = jetty_table_a.lookup(jetty_handle_a.0).unwrap();

        // Write_imm from B to A's MR, notify A's Jetty with imm=42
        let dst_jetty = JettyAddr { node_id: 1, jetty_id: jetty_handle_a.0 };
        engine_b.ub_write_imm_remote(addr_a, &[0xDE, 0xAD, 0xBE, 0xEF], 42, 1, dst_jetty).await.unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify MR was written
        let entry = mr_table_a.lookup(handle_a.0).unwrap();
        let mut buf = [0u8; 4];
        entry.device.read(entry.base_offset, &mut buf).unwrap();
        assert_eq!(&buf, &[0xDE, 0xAD, 0xBE, 0xEF]);

        // Verify CQE on node A's Jetty
        let cqe = jetty_a.poll_cqe().unwrap();
        assert_eq!(cqe.imm, Some(42));
        assert_eq!(cqe.verb, Verb::WriteImm);
        assert_eq!(cqe.byte_len, 4);
    }

    /// Test that two sends on the same (src_jetty, dst_jetty) pair arrive in order.
    #[tokio::test]
    async fn test_message_ordering_same_jetty_pair() {
        let fabric_a = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());
        let fabric_b = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());

        let mr_table_a = Arc::new(MrTable::new(1, 1));
        let mr_cache_a = Arc::new(MrCacheTable::new());
        let mr_table_b = Arc::new(MrTable::new(1, 2));
        let mr_cache_b = Arc::new(MrCacheTable::new());

        let jetty_table_a = Arc::new(JettyTable::new(1, test_jetty_config()));
        let jetty_table_b = Arc::new(JettyTable::new(2, test_jetty_config()));

        let mut engine_a = DataPlaneEngine::new(1, Arc::clone(&mr_table_a), mr_cache_a, Arc::clone(&jetty_table_a), fabric_a);
        let mut engine_b = DataPlaneEngine::new(2, mr_table_b, mr_cache_b, Arc::clone(&jetty_table_b), fabric_b);

        engine_a.start().await.unwrap();
        engine_b.start().await.unwrap();

        let addr_a_dp = engine_a.fabric.local_addr();
        engine_b.connect_peer(1, addr_a_dp).await.unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Create Jetty on A, post 2 recv buffers
        let jetty_handle_a = jetty_table_a.create().unwrap();
        let jetty_a = jetty_table_a.lookup(jetty_handle_a.0).unwrap();

        // Post recv with different wr_ids so we can identify ordering
        jetty_a.post_recv(vec![0u8; 64], 100).unwrap(); // first recv
        jetty_a.post_recv(vec![0u8; 64], 101).unwrap(); // second recv

        // Create Jetty on B for src
        let jetty_handle_b = jetty_table_b.create().unwrap();

        let dst_jetty = JettyAddr { node_id: 1, jetty_id: jetty_handle_a.0 };

        // Send message 1 then message 2
        engine_b.ub_send_remote(jetty_handle_b.0, dst_jetty, &[1]).await.unwrap();
        engine_b.ub_send_remote(jetty_handle_b.0, dst_jetty, &[2]).await.unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;

        // First CQE should be for wr_id=100, second for wr_id=101
        let cqe1 = jetty_a.poll_cqe().expect("first CQE");
        let cqe2 = jetty_a.poll_cqe().expect("second CQE");

        assert_eq!(cqe1.wr_id, 100, "first CQE should match first recv buffer");
        assert_eq!(cqe2.wr_id, 101, "second CQE should match second recv buffer");
    }

    /// Test concurrent sends from multiple tasks to the same Jetty.
    #[tokio::test]
    async fn test_concurrent_sends_same_jetty() {
        let fabric_a = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());
        let fabric_b = Arc::new(UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());

        let mr_table_a = Arc::new(MrTable::new(1, 1));
        let mr_cache_a = Arc::new(MrCacheTable::new());
        let mr_table_b = Arc::new(MrTable::new(1, 2));
        let mr_cache_b = Arc::new(MrCacheTable::new());

        let jetty_table_a = Arc::new(JettyTable::new(1, test_jetty_config()));
        let jetty_table_b = Arc::new(JettyTable::new(2, test_jetty_config()));

        let mut engine_a = DataPlaneEngine::new(1, Arc::clone(&mr_table_a), mr_cache_a, Arc::clone(&jetty_table_a), fabric_a);
        let mut engine_b = DataPlaneEngine::new(2, mr_table_b, mr_cache_b, Arc::clone(&jetty_table_b), fabric_b);

        engine_a.start().await.unwrap();
        engine_b.start().await.unwrap();

        let addr_a_dp = engine_a.fabric.local_addr();
        engine_b.connect_peer(1, addr_a_dp).await.unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Create Jetty on A, post 4 recv buffers
        let jetty_handle_a = jetty_table_a.create().unwrap();
        let jetty_a = jetty_table_a.lookup(jetty_handle_a.0).unwrap();
        for i in 0..4u64 {
            jetty_a.post_recv(vec![0u8; 64], 200 + i).unwrap();
        }

        let jetty_handle_b = jetty_table_b.create().unwrap();
        let dst_jetty = JettyAddr { node_id: 1, jetty_id: jetty_handle_a.0 };

        // Send 4 messages concurrently from B
        let mut handles = Vec::new();
        for i in 0..4u8 {
            let engine = engine_b.clone();
            let dst = dst_jetty;
            handles.push(tokio::spawn(async move {
                engine.ub_send_remote(1, dst, &[i]).await
            }));
        }

        for h in handles {
            h.await.unwrap().unwrap();
        }

        tokio::time::sleep(Duration::from_millis(500)).await;

        // All 4 CQEs should eventually arrive
        let mut cqe_count = 0;
        while jetty_a.poll_cqe().is_some() {
            cqe_count += 1;
        }
        assert_eq!(cqe_count, 4, "all 4 concurrent sends should produce CQEs");
    }
}
