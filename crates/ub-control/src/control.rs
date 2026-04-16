use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use ub_core::config::NodeConfig;
use ub_core::error::UbError;
use ub_core::mr::{MrCacheTable, MrPublishEvent, MrTable};
use ub_core::types::{NodeState, PeerChangeEvent};

use crate::member::{MemberTable, NodeInfo};
use crate::message::{ControlMsg, HelloPayload, HeartbeatPayload, MemberDownPayload, MrPublishPayload, MrRevokePayload};

/// Control plane engine.
pub struct ControlPlane {
    config: Arc<NodeConfig>,
    members: Arc<MemberTable>,
    mr_cache: Arc<MrCacheTable>,
    mr_table: Option<Arc<MrTable>>,
    mr_event_rx: Option<mpsc::UnboundedReceiver<MrPublishEvent>>,
    local_epoch: u32,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    /// Watch channel for notifying data plane about peer state changes.
    peer_change_tx: watch::Sender<PeerChangeEvent>,
    pub peer_change_rx: watch::Receiver<PeerChangeEvent>,
    tasks: Vec<JoinHandle<()>>,
}

impl ControlPlane {
    pub fn new(config: Arc<NodeConfig>, members: Arc<MemberTable>, mr_cache: Arc<MrCacheTable>) -> Self {
        let local_epoch = rand::random::<u32>();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (peer_change_tx, peer_change_rx) = watch::channel(PeerChangeEvent::Joined { node_id: 0, epoch: 0 });
        ControlPlane {
            config,
            members,
            mr_cache,
            mr_table: None,
            mr_event_rx: None,
            local_epoch,
            shutdown_tx,
            shutdown_rx,
            peer_change_tx,
            peer_change_rx,
            tasks: Vec::new(),
        }
    }

    /// Set the local MR table for MR_PUBLISH/MR_REVOKE broadcast.
    /// Takes the event receiver from the MrTable.
    pub fn set_mr_table(&mut self, mr_table: Arc<MrTable>, mr_event_rx: mpsc::UnboundedReceiver<MrPublishEvent>) {
        self.mr_table = Some(mr_table);
        self.mr_event_rx = Some(mr_event_rx);
    }

    pub fn local_epoch(&self) -> u32 {
        self.local_epoch
    }

    pub fn member_table(&self) -> Arc<MemberTable> {
        Arc::clone(&self.members)
    }

    pub fn mr_cache(&self) -> Arc<MrCacheTable> {
        Arc::clone(&self.mr_cache)
    }

    /// Start the control plane: listen for connections, connect to peers, run heartbeat.
    pub async fn start(&mut self) -> Result<(), UbError> {
        let listen_addr: SocketAddr = self.config.control.listen.parse()
            .map_err(|e| UbError::Config(format!("invalid control listen addr: {e}")))?;

        // Start TCP listener
        let listener = TcpListener::bind(listen_addr).await?;
        tracing::info!("control plane listening on {}", listen_addr);

        // Register self in member table
        let self_info = NodeInfo {
            node_id: self.config.node_id,
            state: NodeState::Active,
            control_addr: self.config.control.listen.clone(),
            data_addr: self.config.data.listen.clone(),
            epoch: self.local_epoch,
            initial_credits: self.config.flow.initial_credits,
            last_seen: Some(now_millis()),
            tx: None,
        };
        self.members.upsert(self_info);

        // Accept loop
        let members = Arc::clone(&self.members);
        let config = Arc::clone(&self.config);
        let local_epoch = self.local_epoch;
        let mut shutdown_rx = self.shutdown_rx.clone();
        let mr_cache = Arc::clone(&self.mr_cache);
        let mr_table_for_accept = self.mr_table.clone();
        let peer_change_tx = self.peer_change_tx.clone();

        let accept_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    accept_result = listener.accept() => {
                        match accept_result {
                            Ok((stream, addr)) => {
                                tracing::info!("control plane: accepted connection from {}", addr);
                                let members = Arc::clone(&members);
                                let config = Arc::clone(&config);
                                let mr_cache = Arc::clone(&mr_cache);
                                let mr_table = mr_table_for_accept.clone();
                                let peer_change_tx = peer_change_tx.clone();
                                tokio::spawn(handle_peer_connection(
                                    stream, members, config, local_epoch, mr_cache, mr_table, true, peer_change_tx,
                                ));
                            }
                            Err(e) => {
                                tracing::error!("control plane accept error: {e}");
                            }
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        tracing::info!("control plane accept loop shutting down");
                        break;
                    }
                }
            }
        });
        self.tasks.push(accept_handle);

        // Connect to static peers
        if self.config.control.bootstrap == "static" {
            for peer_addr_str in &self.config.control.peers {
                if let Ok(peer_addr) = peer_addr_str.parse::<SocketAddr>() {
                    let members = Arc::clone(&self.members);
                    let config = Arc::clone(&self.config);
                    let local_epoch = self.local_epoch;
                    let mr_cache = Arc::clone(&self.mr_cache);
                    let mr_table = self.mr_table.clone();
                    let peer_change_tx = self.peer_change_tx.clone();
                    let peer_str = peer_addr_str.clone();
                    tokio::spawn(async move {
                        // Retry connection with backoff
                        let mut delay = Duration::from_secs(1);
                        loop {
                            match TcpStream::connect(peer_addr).await {
                                Ok(stream) => {
                                    tracing::info!("control plane: connected to peer {}", peer_str);
                                    handle_peer_connection(
                                        stream, members, config, local_epoch, mr_cache, mr_table, false, peer_change_tx,
                                    ).await;
                                    break;
                                }
                                Err(e) => {
                                    tracing::warn!("control plane: connect to {} failed: {e}, retrying in {:?}", peer_str, delay);
                                    tokio::time::sleep(delay).await;
                                    delay = std::cmp::min(delay * 2, Duration::from_secs(30));
                                }
                            }
                        }
                    });
                }
            }
        }

        // MR publish event drain loop
        if let Some(mut mr_event_rx) = self.mr_event_rx.take() {
            let members_mr = Arc::clone(&self.members);
            let mut shutdown_rx_mr = self.shutdown_rx.clone();

            let mr_handle = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        event = mr_event_rx.recv() => {
                            match event {
                                Some(MrPublishEvent::Publish(info)) => {
                                    let msg = ControlMsg::MrPublish(MrPublishPayload {
                                        owner_node: info.owner_node,
                                        mr_handle: info.mr_handle,
                                        base_ub_addr: info.base_ub_addr,
                                        len: info.len,
                                        perms: info.perms,
                                        device_kind: info.device_kind,
                                    });
                                    let encoded = msg.encode();
                                    let encoded_bytes = encoded.to_vec();
                                    let senders = members_mr.active_peer_senders();
                                    for (_node_id, tx) in &senders {
                                        let _ = tx.send(encoded_bytes.clone()).await;
                                    }
                                    tracing::info!(
                                        "MR_PUBLISH broadcast: handle={} node={}",
                                        info.mr_handle, info.owner_node
                                    );
                                }
                                Some(MrPublishEvent::Revoke { owner_node, mr_handle }) => {
                                    let msg = ControlMsg::MrRevoke(MrRevokePayload {
                                        owner_node,
                                        mr_handle,
                                    });
                                    let encoded = msg.encode();
                                    let encoded_bytes = encoded.to_vec();
                                    let senders = members_mr.active_peer_senders();
                                    for (_node_id, tx) in &senders {
                                        let _ = tx.send(encoded_bytes.clone()).await;
                                    }
                                    tracing::info!(
                                        "MR_REVOKE broadcast: handle={} node={}",
                                        mr_handle, owner_node
                                    );
                                }
                                None => break,
                            }
                        }
                        _ = shutdown_rx_mr.changed() => {
                            break;
                        }
                    }
                }
            });
            self.tasks.push(mr_handle);
        }

        // Heartbeat loop
        let members_hb = Arc::clone(&self.members);
        let interval_ms = self.config.heartbeat.interval_ms;
        let fail_after = self.config.heartbeat.fail_after;
        let mut shutdown_rx_hb = self.shutdown_rx.clone();
        let peer_change_tx_hb = self.peer_change_tx.clone();

        let hb_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
            let mut missed_count: std::collections::HashMap<u16, u32> = std::collections::HashMap::new();
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let now = now_millis();
                        let msg = ControlMsg::Heartbeat(HeartbeatPayload {
                            node_id: members_hb.local_node_id(),
                            timestamp: now,
                        });
                        let encoded = msg.encode();
                        let encoded_bytes = encoded.to_vec();

                        // Send heartbeat to all active peers
                        let senders = members_hb.active_peer_senders();
                        for (node_id, tx) in &senders {
                            let _ = tx.send(encoded_bytes.clone()).await;
                        }

                        // Check for missed heartbeats
                        let nodes = members_hb.list();
                        for node in &nodes {
                            if node.node_id == members_hb.local_node_id() {
                                continue;
                            }
                            if node.state == NodeState::Down {
                                continue;
                            }
                            if let Some(last_seen) = node.last_seen {
                                if now > last_seen + interval_ms as u64 * fail_after as u64 {
                                    let count = missed_count.entry(node.node_id).or_insert(0);
                                    *count += 1;
                                    if *count >= fail_after {
                                        if node.state == NodeState::Active {
                                            tracing::warn!("node {} missed {} heartbeats, marking as suspect", node.node_id, fail_after);
                                            members_hb.mark_suspect(node.node_id);
                                            let _ = peer_change_tx_hb.send(PeerChangeEvent::Suspect {
                                                node_id: node.node_id,
                                            });
                                        }
                                        if *count >= fail_after * 2 {
                                            tracing::warn!("node {} missed {} heartbeats, marking as down", node.node_id, fail_after * 2);
                                            members_hb.mark_down(node.node_id);
                                            let _ = peer_change_tx_hb.send(PeerChangeEvent::Down {
                                                node_id: node.node_id,
                                                epoch: node.epoch,
                                            });
                                        }
                                    }
                                } else {
                                    // Heartbeat received — recover from Suspect if needed
                                    if missed_count.remove(&node.node_id).is_some() {
                                        if node.state == NodeState::Suspect {
                                            tracing::info!("node {} heartbeat resumed, recovering to Active", node.node_id);
                                            members_hb.mark_active(node.node_id);
                                            let _ = peer_change_tx_hb.send(PeerChangeEvent::Recovered {
                                                node_id: node.node_id,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ = shutdown_rx_hb.changed() => {
                        break;
                    }
                }
            }
        });
        self.tasks.push(hb_handle);

        Ok(())
    }

    /// Graceful shutdown.
    pub async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        // Broadcast MEMBER_DOWN for self
        let msg = ControlMsg::MemberDown(MemberDownPayload {
            node_id: self.config.node_id,
            reason: 0, // graceful
        });
        let encoded = msg.encode();
        let _ = self.members.broadcast(encoded.to_vec()).await;
    }
}

/// Handle a peer TCP connection (both accepted and initiated).
async fn handle_peer_connection(
    stream: TcpStream,
    members: Arc<MemberTable>,
    config: Arc<NodeConfig>,
    local_epoch: u32,
    mr_cache: Arc<MrCacheTable>,
    mr_table: Option<Arc<MrTable>>,
    _is_incoming: bool,
    peer_change_tx: watch::Sender<PeerChangeEvent>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);

    // Create channel for sending messages to this peer
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);

    // Send HELLO
    let hello = ControlMsg::Hello(HelloPayload {
        node_id: config.node_id,
        version: 1,
        local_epoch,
        initial_credits: config.flow.initial_credits,
        data_addr: config.data.listen.clone(),
    });
    let hello_bytes = hello.encode();
    if let Err(e) = write_half.write_all(&hello_bytes).await {
        tracing::error!("failed to send HELLO: {e}");
        return;
    }

    // Read loop
    let members_read = Arc::clone(&members);
    let mr_cache_read = Arc::clone(&mr_cache);
    let config_read = Arc::clone(&config);
    let tx_clone = tx.clone();
    let local_epoch_read = local_epoch;
    let mr_table_read = mr_table.clone();
    let peer_change_tx_read = peer_change_tx.clone();

    let read_handle = tokio::spawn(async move {
        let _buf: Vec<u8> = Vec::new();
        let mut len_buf = [0u8; 4];
        loop {
            // Read length prefix
            if let Err(e) = tokio::io::AsyncReadExt::read_exact(&mut reader, &mut len_buf).await {
                if e.kind() != std::io::ErrorKind::UnexpectedEof {
                    tracing::warn!("control read error: {e}");
                }
                break;
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            if len > 1_000_000 {
                tracing::error!("control message too large: {len}");
                break;
            }

            // Read MsgType + payload
            let mut msg_buf = vec![0u8; 1 + len];
            if let Err(e) = tokio::io::AsyncReadExt::read_exact(&mut reader, &mut msg_buf).await {
                tracing::warn!("control read error: {e}");
                break;
            }

            // Reconstruct full message: len(4) + msg_buf
            let mut full_msg = Vec::with_capacity(5 + len);
            full_msg.extend_from_slice(&len_buf);
            full_msg.extend_from_slice(&msg_buf);

            // Decode and process
            match ControlMsg::decode(&full_msg) {
                Ok(msg) => {
                    process_control_message(
                        msg, &members_read, &config_read, &mr_cache_read, &mr_table_read,
                        &tx_clone, local_epoch_read, &peer_change_tx_read,
                    ).await;
                }
                Err(e) => {
                    tracing::warn!("failed to decode control message: {e}");
                }
            }
        }
    });

    // Write loop — forward messages from channel to TCP
    let write_handle = tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            if let Err(e) = write_half.write_all(&data).await {
                tracing::warn!("control write error: {e}");
                break;
            }
        }
    });

    let _ = read_handle.await;
    write_handle.abort();
}

/// Process a received control message.
async fn process_control_message(
    msg: ControlMsg,
    members: &MemberTable,
    config: &NodeConfig,
    mr_cache: &MrCacheTable,
    mr_table: &Option<Arc<MrTable>>,
    peer_tx: &mpsc::Sender<Vec<u8>>,
    local_epoch: u32,
    peer_change_tx: &watch::Sender<PeerChangeEvent>,
) {
    match msg {
        ControlMsg::Hello(hello) => {
            tracing::info!(
                "HELLO from node {} epoch {} data_addr={}",
                hello.node_id, hello.local_epoch, hello.data_addr
            );
            // Register peer in member table
            let info = NodeInfo {
                node_id: hello.node_id,
                state: NodeState::Active,
                control_addr: String::new(),
                data_addr: hello.data_addr,
                epoch: hello.local_epoch,
                initial_credits: hello.initial_credits,
                last_seen: Some(now_millis()),
                tx: Some(peer_tx.clone()),
            };
            members.upsert(info);

            // Send HelloAck
            let ack = ControlMsg::HelloAck(HelloPayload {
                node_id: config.node_id,
                version: 1,
                local_epoch,
                initial_credits: config.flow.initial_credits,
                data_addr: config.data.listen.clone(),
            });
            let encoded = ack.encode();
            let _ = peer_tx.send(encoded.to_vec()).await;

            // Send existing MRs to the new peer
            send_existing_mrs(mr_table, peer_tx).await;

            // Notify data plane about new peer
            let _ = peer_change_tx.send(PeerChangeEvent::Joined {
                node_id: hello.node_id,
                epoch: hello.local_epoch,
            });
        }
        ControlMsg::HelloAck(hello) => {
            tracing::info!(
                "HELLO_ACK from node {} epoch {} data_addr={}",
                hello.node_id, hello.local_epoch, hello.data_addr
            );
            let info = NodeInfo {
                node_id: hello.node_id,
                state: NodeState::Active,
                control_addr: String::new(),
                data_addr: hello.data_addr,
                epoch: hello.local_epoch,
                initial_credits: hello.initial_credits,
                last_seen: Some(now_millis()),
                tx: Some(peer_tx.clone()),
            };
            members.upsert(info);

            // Send existing MRs to the new peer
            send_existing_mrs(mr_table, peer_tx).await;

            // Notify data plane about new peer
            let _ = peer_change_tx.send(PeerChangeEvent::Joined {
                node_id: hello.node_id,
                epoch: hello.local_epoch,
            });
        }
        ControlMsg::Heartbeat(hb) => {
            members.update_last_seen(hb.node_id, now_millis());
            // Respond with HeartbeatAck echoing the timestamp, including our own node_id
            let ack = ControlMsg::HeartbeatAck(HeartbeatPayload {
                node_id: config.node_id,
                timestamp: hb.timestamp,
            });
            let encoded = ack.encode();
            let _ = peer_tx.send(encoded.to_vec()).await;
        }
        ControlMsg::HeartbeatAck(hb) => {
            members.update_last_seen(hb.node_id, now_millis());
        }
        ControlMsg::MemberDown(md) => {
            tracing::info!("MEMBER_DOWN: node {} reason={}", md.node_id, md.reason);
            members.mark_down(md.node_id);
            let epoch = members.get(md.node_id).map(|n| n.epoch).unwrap_or(0);
            let _ = peer_change_tx.send(PeerChangeEvent::Down {
                node_id: md.node_id,
                epoch,
            });
        }
        ControlMsg::MrPublish(mr) => {
            tracing::info!(
                "MR_PUBLISH: node={} handle={} len={}",
                mr.owner_node, mr.mr_handle, mr.len
            );
            // Add to remote MR cache
            let entry = ub_core::mr::MrCacheEntry {
                remote_mr_handle: mr.mr_handle,
                owner_node: mr.owner_node,
                base_ub_addr: mr.base_ub_addr,
                len: mr.len,
                perms: mr.perms,
                device_kind: mr.device_kind,
            };
            mr_cache.insert(entry);
        }
        ControlMsg::MrRevoke(revoke) => {
            tracing::info!("MR_REVOKE: node={} handle={}", revoke.owner_node, revoke.mr_handle);
            mr_cache.remove(revoke.owner_node, revoke.mr_handle);
        }
        _ => {
            tracing::warn!("unhandled control message type");
        }
    }
}

/// Send all existing local MRs to a newly connected peer via MR_PUBLISH.
async fn send_existing_mrs(
    mr_table: &Option<Arc<MrTable>>,
    peer_tx: &mpsc::Sender<Vec<u8>>,
) {
    if let Some(ref table) = mr_table {
        let entries = table.list();
        for entry in entries {
            let msg = ControlMsg::MrPublish(MrPublishPayload {
                owner_node: entry.base_ub_addr.node_id(),
                mr_handle: entry.handle,
                base_ub_addr: entry.base_ub_addr,
                len: entry.len,
                perms: entry.perms,
                device_kind: entry.device.kind(),
            });
            let encoded = msg.encode();
            if let Err(e) = peer_tx.send(encoded.to_vec()).await {
                tracing::warn!("failed to send MR_PUBLISH to new peer: {e}");
                break;
            }
        }
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_now_millis() {
        let t = now_millis();
        assert!(t > 0);
    }

    #[tokio::test]
    async fn test_peer_change_event_channel() {
        let (tx, mut rx) = watch::channel(PeerChangeEvent::Joined { node_id: 0, epoch: 0 });

        // Send Joined event
        tx.send(PeerChangeEvent::Joined { node_id: 2, epoch: 100 }).unwrap();
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), PeerChangeEvent::Joined { node_id: 2, epoch: 100 });

        // Send Suspect event
        tx.send(PeerChangeEvent::Suspect { node_id: 2 }).unwrap();
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), PeerChangeEvent::Suspect { node_id: 2 });

        // Send Recovered event
        tx.send(PeerChangeEvent::Recovered { node_id: 2 }).unwrap();
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), PeerChangeEvent::Recovered { node_id: 2 });

        // Send Down event
        tx.send(PeerChangeEvent::Down { node_id: 2, epoch: 100 }).unwrap();
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), PeerChangeEvent::Down { node_id: 2, epoch: 100 });
    }
}
