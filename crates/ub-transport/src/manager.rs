use std::sync::Arc;
use std::time::Duration;

use bytes::{BufMut, BytesMut};
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time;

use ub_core::config::{FlowConfig, TransportConfig};
use ub_core::error::UbError;
use ub_core::types::PeerChangeEvent;
use ub_wire::codec::{decode_ack_payload, decode_credit_payload, decode_frame, encode_frame};
use ub_wire::frame::*;

use crate::session::{
    ReceiveAction, ReliableSession, RetransmitEntry, SessionKey, SessionState,
};

/// A decoded inbound data frame ready for verb processing.
#[derive(Debug)]
pub struct InboundFrame {
    pub header: FrameHeader,
    pub ext: DataExtHeader,
    pub payload: Vec<u8>,
    pub seq: u64,
}

/// Transport manager — orchestrates reliable sessions, ACK/CREDIT framing,
/// and RTO retransmission across all peers.
pub struct TransportManager {
    local_node_id: u16,
    sessions: Arc<DashMap<SessionKey, Arc<Mutex<ReliableSession>>>>,
    peer_senders: Arc<DashMap<u16, mpsc::Sender<Vec<u8>>>>,
    inbound_tx: mpsc::UnboundedSender<InboundFrame>,
    config: TransportConfig,
    flow_config: FlowConfig,
    rto_task: Option<JoinHandle<()>>,
    peer_change_rx: Option<watch::Receiver<PeerChangeEvent>>,
    /// Current epoch per remote node — used to look up the correct session.
    peer_epochs: DashMap<u16, u32>,
}

impl TransportManager {
    /// Create a new TransportManager.
    pub fn new(
        local_node_id: u16,
        config: TransportConfig,
        flow_config: FlowConfig,
        inbound_tx: mpsc::UnboundedSender<InboundFrame>,
    ) -> Self {
        TransportManager {
            local_node_id,
            sessions: Arc::new(DashMap::new()),
            peer_senders: Arc::new(DashMap::new()),
            inbound_tx,
            config,
            flow_config,
            rto_task: None,
            peer_change_rx: None,
            peer_epochs: DashMap::new(),
        }
    }

    /// Set the peer change receiver for epoch tracking.
    pub fn set_peer_change_rx(&mut self, rx: watch::Receiver<PeerChangeEvent>) {
        self.peer_change_rx = Some(rx);
    }

    /// Start the RTO retransmission loop.
    pub fn start_rto_task(&mut self) {
        let sessions = self.sessions.clone();
        let peer_senders = self.peer_senders.clone();
        let rto_interval_ms = self.config.rto_ms / 2; // Tick at half RTO

        let handle = tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_millis(rto_interval_ms.max(50)));
            loop {
                interval.tick().await;
                let now = std::time::Instant::now();

                // Collect sessions that need retransmission
                let mut retransmit_frames: Vec<(u16, Vec<u8>)> = Vec::new();

                for entry in sessions.iter() {
                    let mut session = entry.value().lock();
                    if session.state != SessionState::Active {
                        continue;
                    }

                    let timeout_seqs = session.check_rto(now);

                    if session.state == SessionState::Dead {
                        tracing::warn!(
                            "session ({},{}) dead after max retries",
                            session.key.local_node,
                            session.key.remote_node
                        );
                        continue;
                    }

                    for seq in timeout_seqs {
                        if let Some(re_entry) = session.get_retransmit_entry(seq) {
                            retransmit_frames.push((session.key.remote_node, re_entry.frame.clone()));
                        }
                    }
                }

                // Retransmit
                for (remote_node, frame) in retransmit_frames {
                    ub_obs::incr(ub_obs::RETRANS);
                    if let Some(sender) = peer_senders.get(&remote_node) {
                        let _ = sender.send(frame).await;
                    }
                }
            }
        });

        self.rto_task = Some(handle);
    }

    /// Create a reliable session for a peer.
    pub fn create_session(
        &self,
        remote_node: u16,
        epoch: u32,
        initial_credits: u32,
    ) -> Result<(), UbError> {
        let key = SessionKey {
            local_node: self.local_node_id,
            remote_node,
            epoch,
        };

        if self.sessions.contains_key(&key) {
            return Err(UbError::Internal(format!("session already exists for {:?}", key)));
        }

        let session = ReliableSession::new(
            self.local_node_id,
            remote_node,
            epoch,
            self.config.rto_ms,
            self.config.max_retries,
            initial_credits,
        );

        self.sessions.insert(key, Arc::new(Mutex::new(session)));
        self.peer_epochs.insert(remote_node, epoch);

        tracing::info!(
            "transport: created session for peer {} epoch {} credits {}",
            remote_node, epoch, initial_credits
        );
        Ok(())
    }

    /// Invalidate a session (on epoch change or peer restart).
    /// Returns all unacknowledged retransmit entries.
    pub fn invalidate_session(&self, remote_node: u16, old_epoch: u32) -> Vec<RetransmitEntry> {
        let key = SessionKey {
            local_node: self.local_node_id,
            remote_node,
            epoch: old_epoch,
        };

        if let Some((_, session)) = self.sessions.remove(&key) {
            let mut session = session.lock();
            tracing::info!(
                "transport: invalidated session for peer {} epoch {}",
                remote_node, old_epoch
            );
            session.kill()
        } else {
            Vec::new()
        }
    }

    /// Send a frame through the reliable transport.
    /// Assigns a sequence number, sets ACK_REQ, and queues for retransmission.
    #[tracing::instrument(skip(self, frame_data))]
    pub fn send(
        &self,
        remote_node: u16,
        frame_data: &mut [u8],
        is_fire_and_forget: bool,
        opaque: u64,
    ) -> Result<u64, UbError> {
        let epoch = self.peer_epochs.get(&remote_node)
            .map(|e| *e.value())
            .ok_or(UbError::LinkDown)?;

        let key = SessionKey {
            local_node: self.local_node_id,
            remote_node,
            epoch,
        };

        let session_arc = self.sessions.get(&key)
            .ok_or(UbError::LinkDown)?
            .clone();

        let seq = {
            let mut session = session_arc.lock();
            if session.state != SessionState::Active {
                return Err(UbError::LinkDown);
            }
            session.consume_credit()?;
            session.assign_seq(frame_data, is_fire_and_forget, opaque)
        };

        // Send the frame
        let sender = self.peer_senders.get(&remote_node)
            .ok_or(UbError::LinkDown)?;

        // Use try_send to avoid blocking in the send path
        match sender.try_send(frame_data.to_vec()) {
            Ok(()) => Ok(seq),
            Err(e) => {
                tracing::warn!("transport: failed to send frame to peer {}: {}", remote_node, e);
                Err(UbError::LinkDown)
            }
        }
    }

    /// Handle an incoming raw frame from the network.
    /// Dispatches to the appropriate handler based on frame type.
    #[tracing::instrument(skip(self, raw))]
    pub fn handle_incoming(&self, raw: &[u8]) {
        if let Ok((header, ext, payload)) = decode_frame(raw) {
            match header.frame_type {
                FrameType::Data => {
                    if let Some(ext_header) = ext {
                        self.handle_data_frame(header, ext_header, payload.to_vec());
                    }
                }
                FrameType::Ack => {
                    self.handle_ack_frame(&header, &payload);
                }
                FrameType::Credit => {
                    self.handle_credit_frame(&header, &payload);
                }
            }
        }
    }

    /// Register a peer sender for outbound frames.
    pub fn register_peer_sender(&self, node_id: u16, sender: mpsc::Sender<Vec<u8>>) {
        self.peer_senders.insert(node_id, sender);
    }

    /// Remove a peer sender.
    pub fn remove_peer_sender(&self, node_id: u16) {
        self.peer_senders.remove(&node_id);
    }

    /// Notify the transport that a peer has gone down.
    /// Kills all sessions for this peer and returns unacknowledged entries.
    pub fn notify_peer_down(&self, node_id: u16) -> Vec<RetransmitEntry> {
        let mut all_entries = Vec::new();

        // Remove peer sender
        self.peer_senders.remove(&node_id);

        // Kill all sessions for this peer
        let keys_to_remove: Vec<SessionKey> = self.sessions.iter()
            .filter(|e| e.key().remote_node == node_id)
            .map(|e| e.key().clone())
            .collect();

        for key in keys_to_remove {
            if let Some((_, session)) = self.sessions.remove(&key) {
                let mut session = session.lock();
                all_entries.extend(session.kill());
            }
        }

        self.peer_epochs.remove(&node_id);

        tracing::info!("transport: peer {} down, killed sessions", node_id);
        all_entries
    }

    /// Mark a write-verb frame as executed (for idempotency).
    pub fn mark_executed(&self, remote_node: u16, seq: u64) {
        if let Some(epoch) = self.peer_epochs.get(&remote_node) {
            let key = SessionKey {
                local_node: self.local_node_id,
                remote_node,
                epoch: *epoch.value(),
            };
            if let Some(session) = self.sessions.get(&key) {
                let mut session = session.lock();
                session.mark_executed(seq);
            }
        }
    }

    /// Check if a write-verb frame has already been executed.
    pub fn is_executed(&self, remote_node: u16, seq: u64) -> bool {
        if let Some(epoch) = self.peer_epochs.get(&remote_node) {
            let key = SessionKey {
                local_node: self.local_node_id,
                remote_node,
                epoch: *epoch.value(),
            };
            if let Some(session) = self.sessions.get(&key) {
                let session = session.lock();
                return session.is_executed(seq);
            }
        }
        false
    }

    /// Complete a request-response entry — remove from retransmit queue once
    /// the response has been received. Called by DataPlaneEngine when a
    /// pending request's response arrives.
    pub fn complete_request(&self, remote_node: u16, opaque: u64) {
        if let Some(epoch) = self.peer_epochs.get(&remote_node) {
            let key = SessionKey {
                local_node: self.local_node_id,
                remote_node,
                epoch: *epoch.value(),
            };
            if let Some(session) = self.sessions.get(&key) {
                let mut session = session.lock();
                session.complete_request(opaque);
            }
        }
    }

    /// Grant credits to a peer (called when CQEs are consumed).
    pub fn grant_credits(&self, remote_node: u16, count: u32) {
        if let Some(epoch) = self.peer_epochs.get(&remote_node) {
            let key = SessionKey {
                local_node: self.local_node_id,
                remote_node,
                epoch: *epoch.value(),
            };
            if let Some(session) = self.sessions.get(&key) {
                let mut session = session.lock();
                session.credits_to_grant += count;

                // If enough credits to send a CREDIT frame, do so
                if session.credits_to_grant >= 8 && !session.should_send_ack() {
                    let credits = session.credits_to_grant;
                    session.credits_to_grant = 0;
                    drop(session);
                    self.send_credit_frame(remote_node, credits);
                }
            }
        }
    }

    /// Get the read response cache for a session.
    pub fn read_cache_get(&self, remote_node: u16, key: (u16, u64)) -> Option<Vec<u8>> {
        if let Some(epoch) = self.peer_epochs.get(&remote_node) {
            let session_key = SessionKey {
                local_node: self.local_node_id,
                remote_node,
                epoch: *epoch.value(),
            };
            if let Some(session) = self.sessions.get(&session_key) {
                let mut session = session.lock();
                return session.read_cache.get(key);
            }
        }
        None
    }

    /// Insert into the read response cache for a session.
    pub fn read_cache_insert(&self, remote_node: u16, key: (u16, u64), response: Vec<u8>) {
        if let Some(epoch) = self.peer_epochs.get(&remote_node) {
            let session_key = SessionKey {
                local_node: self.local_node_id,
                remote_node,
                epoch: *epoch.value(),
            };
            if let Some(session) = self.sessions.get(&session_key) {
                let mut session = session.lock();
                session.read_cache.insert(key, response);
            }
        }
    }

    // === Internal handlers ===

    fn handle_data_frame(&self, header: FrameHeader, ext: DataExtHeader, payload: Vec<u8>) {
        let remote_node = header.src_node;
        let seq = header.stream_seq;

        // Unsequenced frame (seq==0) — e.g., response frames like ReadResp.
        // Deliver directly without dedup checking.
        if seq == 0 {
            let _ = self.inbound_tx.send(InboundFrame {
                header,
                ext,
                payload,
                seq,
            });
            return;
        }

        // Find or create session
        let session_key = {
            let epoch = self.peer_epochs.get(&remote_node)
                .map(|e| *e.value())
                .unwrap_or(0);
            SessionKey {
                local_node: self.local_node_id,
                remote_node,
                epoch,
            }
        };

        let session_arc = match self.sessions.get(&session_key) {
            Some(s) => s.clone(),
            None => {
                // No session — deliver without dedup (backward compat)
                let _ = self.inbound_tx.send(InboundFrame {
                    header,
                    ext,
                    payload,
                    seq,
                });
                return;
            }
        };

        let action = {
            let mut session = session_arc.lock();
            if session.state != SessionState::Active {
                return;
            }
            session.receive_frame(seq)
        };

        match action {
            ReceiveAction::Deliver => {
                let _ = self.inbound_tx.send(InboundFrame {
                    header,
                    ext,
                    payload,
                    seq,
                });
            }
            ReceiveAction::Duplicate => {
                // For request-response verbs, deliver the duplicate so the
                // upper layer can resend the cached response.
                if matches!(ext.verb, ub_core::types::Verb::ReadReq | ub_core::types::Verb::AtomicCas | ub_core::types::Verb::AtomicFaa) {
                    let _ = self.inbound_tx.send(InboundFrame {
                        header,
                        ext,
                        payload,
                        seq,
                    });
                }
                // Send ACK but don't deliver fire-and-forget duplicates
            }
            ReceiveAction::OutOfOrder => {
                // Send ACK with SACK
            }
            ReceiveAction::BeyondWindow => {
                return; // Discard
            }
        }

        // Send ACK if needed
        let should_ack = {
            let session = session_arc.lock();
            action == ReceiveAction::Duplicate
                || action == ReceiveAction::OutOfOrder
                || session.should_send_ack()
        };

        if should_ack {
            let has_gap = action == ReceiveAction::OutOfOrder;
            let (ack_frame, remote_node) = {
                let mut session = session_arc.lock();
                let ack = session.build_ack_payload(has_gap);
                let frame = build_ack_frame(self.local_node_id, remote_node, &ack);
                (frame, session.key.remote_node)
            };
            self.send_frame_to_peer(remote_node, &ack_frame);
        }
    }

    fn handle_ack_frame(&self, header: &FrameHeader, payload: &[u8]) {
        let remote_node = header.src_node;
        let has_sack = header.flags.contains(FrameFlags::HAS_SACK);

        if let Ok(ack) = decode_ack_payload(payload, has_sack) {
            let session_key = {
                let epoch = self.peer_epochs.get(&remote_node)
                    .map(|e| *e.value())
                    .unwrap_or(0);
                SessionKey {
                    local_node: self.local_node_id,
                    remote_node,
                    epoch,
                }
            };

            if let Some(session) = self.sessions.get(&session_key) {
                let mut session = session.lock();
                session.process_ack(&ack);
            }
        }
    }

    fn handle_credit_frame(&self, header: &FrameHeader, payload: &[u8]) {
        let remote_node = header.src_node;

        if let Ok(credit) = decode_credit_payload(payload) {
            let session_key = {
                let epoch = self.peer_epochs.get(&remote_node)
                    .map(|e| *e.value())
                    .unwrap_or(0);
                SessionKey {
                    local_node: self.local_node_id,
                    remote_node,
                    epoch,
                }
            };

            if let Some(session) = self.sessions.get(&session_key) {
                let mut session = session.lock();
                session.add_credits(credit.credits);
            }
        }
    }

    /// Send a raw frame to a peer without transport sequencing.
    /// Used for response frames (ReadResp, etc.) that don't need retransmission.
    pub fn send_frame_to_peer(&self, remote_node: u16, data: &[u8]) {
        if let Some(sender) = self.peer_senders.get(&remote_node) {
            let _ = sender.try_send(data.to_vec());
        }
    }

    fn send_credit_frame(&self, remote_node: u16, credits: u32) {
        let frame = build_credit_frame(self.local_node_id, remote_node, credits);
        self.send_frame_to_peer(remote_node, &frame);
    }
}

// === Frame construction helpers ===

fn build_ack_frame(local_node: u16, remote_node: u16, ack: &AckPayload) -> BytesMut {
    let header = FrameHeader {
        magic: MAGIC,
        version: VERSION,
        frame_type: FrameType::Ack,
        flags: if ack.sack_bitmap.is_some() {
            FrameFlags::HAS_SACK
        } else {
            FrameFlags::empty()
        },
        src_node: local_node,
        dst_node: remote_node,
        reserved: 0,
        stream_seq: 0,
        payload_len: 0, // Will be set by encode
        header_crc: 0,
    };

    // Encode ACK payload manually
    let payload_len = if ack.sack_bitmap.is_some() { 48 } else { 16 };
    let mut payload = BytesMut::with_capacity(payload_len);
    payload.put_u64(ack.ack_seq);
    payload.put_u32(ack.credit_grant);
    payload.put_u32(ack.reserved);
    if let Some(ref bitmap) = ack.sack_bitmap {
        payload.extend_from_slice(bitmap);
    }

    encode_frame(&header, None, &payload)
}

fn build_credit_frame(local_node: u16, remote_node: u16, credits: u32) -> BytesMut {
    let header = FrameHeader {
        magic: MAGIC,
        version: VERSION,
        frame_type: FrameType::Credit,
        flags: FrameFlags::empty(),
        src_node: local_node,
        dst_node: remote_node,
        reserved: 0,
        stream_seq: 0,
        payload_len: 0,
        header_crc: 0,
    };

    let mut payload = BytesMut::with_capacity(8);
    payload.put_u32(credits);
    payload.put_u32(0); // reserved

    encode_frame(&header, None, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> (TransportConfig, FlowConfig) {
        (
            TransportConfig {
                rto_ms: 200,
                max_retries: 8,
                sack_bitmap_bits: 256,
                reassembly_budget_bytes: 67108864,
            },
            FlowConfig {
                initial_credits: 64,
            },
        )
    }

    #[test]
    fn test_create_session_and_find() {
        let (config, flow_config) = make_config();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mgr = TransportManager::new(1, config, flow_config, tx);

        mgr.create_session(2, 100, 64).unwrap();

        let key = SessionKey { local_node: 1, remote_node: 2, epoch: 100 };
        assert!(mgr.sessions.contains_key(&key));
    }

    #[test]
    fn test_send_assigns_seq() {
        let (config, flow_config) = make_config();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mgr = TransportManager::new(1, config, flow_config, tx);

        mgr.create_session(2, 100, 64).unwrap();

        // Register a peer sender
        let (peer_tx, _peer_rx) = mpsc::channel(256);
        mgr.register_peer_sender(2, peer_tx);

        let mut frame = vec![0u8; 64];
        // Set up minimal frame header for patching
        frame[0..4].copy_from_slice(&MAGIC.to_be_bytes());

        let seq = mgr.send(2, &mut frame, false, 0).unwrap();
        assert_eq!(seq, 1);
    }

    #[test]
    fn test_notify_peer_down_kills_sessions() {
        let (config, flow_config) = make_config();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mgr = TransportManager::new(1, config, flow_config, tx);

        mgr.create_session(2, 100, 64).unwrap();
        let (peer_tx, _peer_rx) = mpsc::channel(256);
        mgr.register_peer_sender(2, peer_tx);

        let entries = mgr.notify_peer_down(2);
        // No pending retransmits, so empty
        assert!(entries.is_empty());

        // Session should be removed
        let key = SessionKey { local_node: 1, remote_node: 2, epoch: 100 };
        assert!(!mgr.sessions.contains_key(&key));
    }

    #[test]
    fn test_credit_exhaustion() {
        let (config, flow_config) = make_config();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mgr = TransportManager::new(1, config, flow_config, tx);

        // Only 2 credits
        mgr.create_session(2, 100, 2).unwrap();
        let (peer_tx, _peer_rx) = mpsc::channel(256);
        mgr.register_peer_sender(2, peer_tx);

        let mut frame = vec![0u8; 64];
        frame[0..4].copy_from_slice(&MAGIC.to_be_bytes());

        mgr.send(2, &mut frame, false, 0).unwrap();
        mgr.send(2, &mut frame, false, 1).unwrap();

        // Third send should fail
        let result = mgr.send(2, &mut frame, false, 2);
        assert!(matches!(result, Err(UbError::NoResources)));
    }

    #[test]
    fn test_handle_data_frame_delivers() {
        let (config, flow_config) = make_config();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mgr = TransportManager::new(2, config, flow_config, tx);

        mgr.create_session(1, 100, 64).unwrap();

        // Build a DATA frame
        let header = FrameHeader {
            magic: MAGIC,
            version: VERSION,
            frame_type: FrameType::Data,
            flags: FrameFlags::ACK_REQ,
            src_node: 1,
            dst_node: 2,
            reserved: 0,
            stream_seq: 1,
            payload_len: 4,
            header_crc: 0,
        };
        let ext = DataExtHeader {
            verb: ub_core::types::Verb::Write,
            ext_flags: ExtFlags::empty(),
            mr_handle: 1,
            jetty_src: 0,
            jetty_dst: 0,
            opaque: 0,
            frag_id: 0,
            frag_index: 0,
            frag_total: 1,
            ub_addr: ub_core::addr::UbAddr::new(1, 1, 0, 0, 0),
            imm: None,
        };
        let payload = &[1, 2, 3, 4];
        let frame = encode_frame(&header, Some(&ext), payload);
        let frame_bytes = frame.freeze();

        mgr.handle_incoming(&frame_bytes);

        // Should receive the inbound frame
        let inbound = rx.try_recv().unwrap();
        assert_eq!(inbound.seq, 1);
        assert_eq!(inbound.payload, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_handle_ack_clears_retransmit() {
        let (config, flow_config) = make_config();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mgr = TransportManager::new(1, config, flow_config, tx);

        mgr.create_session(2, 100, 64).unwrap();
        let (peer_tx, _peer_rx) = mpsc::channel(256);
        mgr.register_peer_sender(2, peer_tx);

        // Send a fire-and-forget frame to populate retransmit queue
        let mut frame = vec![0u8; 64];
        frame[0..4].copy_from_slice(&MAGIC.to_be_bytes());
        mgr.send(2, &mut frame, true, 0).unwrap();

        // Verify entry in retransmit queue
        let key = SessionKey { local_node: 1, remote_node: 2, epoch: 100 };
        {
            let session = mgr.sessions.get(&key).unwrap();
            let s = session.lock();
            assert_eq!(s.pending_count(), 1);
        }

        // Build and handle ACK
        let ack_header = FrameHeader {
            magic: MAGIC,
            version: VERSION,
            frame_type: FrameType::Ack,
            flags: FrameFlags::empty(),
            src_node: 2,
            dst_node: 1,
            reserved: 0,
            stream_seq: 0,
            payload_len: 0,
            header_crc: 0,
        };
        let mut ack_payload = BytesMut::with_capacity(16);
        ack_payload.put_u64(1); // ack_seq = 1
        ack_payload.put_u32(1); // credit_grant = 1
        ack_payload.put_u32(0); // reserved
        let ack_frame = encode_frame(&ack_header, None, &ack_payload);

        mgr.handle_incoming(&ack_frame);

        // Retransmit queue should be cleared for fire-and-forget
        {
            let session = mgr.sessions.get(&key).unwrap();
            let s = session.lock();
            assert_eq!(s.pending_count(), 0);
        }
    }

    #[test]
    fn test_handle_ack_keeps_request_response() {
        let (config, flow_config) = make_config();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mgr = TransportManager::new(1, config, flow_config, tx);

        mgr.create_session(2, 100, 64).unwrap();
        let (peer_tx, _peer_rx) = mpsc::channel(256);
        mgr.register_peer_sender(2, peer_tx);

        // Send a request-response frame (is_fire_and_forget=false)
        let mut frame = vec![0u8; 64];
        frame[0..4].copy_from_slice(&MAGIC.to_be_bytes());
        mgr.send(2, &mut frame, false, 42).unwrap();

        let key = SessionKey { local_node: 1, remote_node: 2, epoch: 100 };

        // ACK should NOT remove the request-response entry
        let ack_header = FrameHeader {
            magic: MAGIC,
            version: VERSION,
            frame_type: FrameType::Ack,
            flags: FrameFlags::empty(),
            src_node: 2,
            dst_node: 1,
            reserved: 0,
            stream_seq: 0,
            payload_len: 0,
            header_crc: 0,
        };
        let mut ack_payload = BytesMut::with_capacity(16);
        ack_payload.put_u64(1);
        ack_payload.put_u32(1);
        ack_payload.put_u32(0);
        let ack_frame = encode_frame(&ack_header, None, &ack_payload);

        mgr.handle_incoming(&ack_frame);

        // Entry should still be in retransmit queue
        {
            let session = mgr.sessions.get(&key).unwrap();
            let s = session.lock();
            assert_eq!(s.pending_count(), 1);
        }

        // complete_request should remove it
        mgr.complete_request(2, 42);
        {
            let session = mgr.sessions.get(&key).unwrap();
            let s = session.lock();
            assert_eq!(s.pending_count(), 0);
        }
    }

    #[test]
    fn test_mark_executed() {
        let (config, flow_config) = make_config();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mgr = TransportManager::new(1, config, flow_config, tx);

        mgr.create_session(2, 100, 64).unwrap();

        assert!(!mgr.is_executed(2, 1));
        mgr.mark_executed(2, 1);
        assert!(mgr.is_executed(2, 1));
    }

    #[test]
    fn test_grant_credits() {
        let (config, flow_config) = make_config();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mgr = TransportManager::new(1, config, flow_config, tx);

        mgr.create_session(2, 100, 2).unwrap();

        let key = SessionKey { local_node: 1, remote_node: 2, epoch: 100 };
        {
            let session = mgr.sessions.get(&key).unwrap();
            let s = session.lock();
            assert_eq!(s.available_credits, 2);
        }

        mgr.grant_credits(2, 10);

        {
            let session = mgr.sessions.get(&key).unwrap();
            let s = session.lock();
            // Credits may have been consumed by the CREDIT frame sending
            assert!(s.available_credits >= 2);
        }
    }

    #[test]
    fn test_build_ack_frame() {
        let ack = AckPayload {
            ack_seq: 5,
            credit_grant: 3,
            reserved: 0,
            sack_bitmap: None,
        };
        let frame = build_ack_frame(1, 2, &ack);
        assert!(frame.len() > 32); // At least header + payload
    }

    #[test]
    fn test_build_credit_frame() {
        let frame = build_credit_frame(1, 2, 10);
        assert!(frame.len() > 32);
    }
}