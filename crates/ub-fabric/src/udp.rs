use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::BytesMut;
use dashmap::DashMap;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};

use ub_core::error::UbError;

use crate::fabric::{Fabric, Listener, Session};
use crate::peer_addr::PeerAddr;

const UDP_RECV_BUF_SIZE: usize = 65536;
const UDP_CHANNEL_SIZE: usize = 1024;

/// UDP-based Fabric implementation.
pub struct UdpFabric {
    socket: Arc<UdpSocket>,
    demux: Arc<DashMap<SocketAddr, mpsc::Sender<BytesMut>>>,
    /// Pending packets from new peers that arrived before accept() created a session.
    pending: Arc<DashMap<SocketAddr, Vec<BytesMut>>>,
    new_peer_tx: mpsc::UnboundedSender<SocketAddr>,
    new_peer_rx: Arc<Mutex<mpsc::UnboundedReceiver<SocketAddr>>>,
}

impl UdpFabric {
    pub async fn bind(addr: SocketAddr) -> Result<Self, UbError> {
        let socket = UdpSocket::bind(addr).await?;
        let (new_peer_tx, new_peer_rx) = mpsc::unbounded_channel();
        let demux: Arc<DashMap<SocketAddr, mpsc::Sender<BytesMut>>> = Arc::new(DashMap::new());
        let pending: Arc<DashMap<SocketAddr, Vec<BytesMut>>> = Arc::new(DashMap::new());
        let socket = Arc::new(socket);

        // Spawn recv loop
        let sock = Arc::clone(&socket);
        let demux_clone = Arc::clone(&demux);
        let pending_clone = Arc::clone(&pending);
        let tx_clone = new_peer_tx.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; UDP_RECV_BUF_SIZE];
            loop {
                match sock.recv_from(&mut buf).await {
                    Ok((len, src_addr)) => {
                        let pkt = BytesMut::from(&buf[..len]);
                        if let Some(sender) = demux_clone.get(&src_addr) {
                            if sender.send(pkt).await.is_err() {
                                demux_clone.remove(&src_addr);
                            }
                        } else {
                            // New peer — buffer packet and notify
                            pending_clone
                                .entry(src_addr)
                                .or_insert_with(Vec::new)
                                .push(pkt);
                            // Only notify once per new peer
                            if !demux_clone.contains_key(&src_addr) {
                                let _ = tx_clone.send(src_addr);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("UDP recv_from error: {e}");
                    }
                }
            }
        });

        Ok(UdpFabric {
            socket,
            demux,
            pending,
            new_peer_tx,
            new_peer_rx: Arc::new(Mutex::new(new_peer_rx)),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }
}

/// UdpListener — waits for new peer sessions.
pub struct UdpListener {
    demux: Arc<DashMap<SocketAddr, mpsc::Sender<BytesMut>>>,
    pending: Arc<DashMap<SocketAddr, Vec<BytesMut>>>,
    socket: Arc<UdpSocket>,
    new_peer_rx: Arc<Mutex<mpsc::UnboundedReceiver<SocketAddr>>>,
}

#[async_trait]
impl Listener for UdpListener {
    async fn accept(&mut self) -> Result<Box<dyn Session>, UbError> {
        let mut rx = self.new_peer_rx.lock().await;
        match rx.recv().await {
            Some(addr) => {
                // Create a new channel for this peer's session
                let (new_tx, new_rx) = mpsc::channel(UDP_CHANNEL_SIZE);
                self.demux.insert(addr, new_tx);

                // Deliver any pending packets that arrived before accept()
                if let Some((_, pending_pkts)) = self.pending.remove(&addr) {
                    for pkt in pending_pkts {
                        if let Some(sender) = self.demux.get(&addr) {
                            if sender.send(pkt).await.is_err() {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                }

                Ok(Box::new(UdpSession {
                    socket: Arc::clone(&self.socket),
                    peer_addr: addr,
                    inbound_rx: Arc::new(Mutex::new(new_rx)),
                }))
            }
            None => Err(UbError::Internal("listener closed".into())),
        }
    }
}

pub struct UdpSession {
    socket: Arc<UdpSocket>,
    peer_addr: SocketAddr,
    inbound_rx: Arc<Mutex<mpsc::Receiver<BytesMut>>>,
}

#[async_trait]
impl Session for UdpSession {
    fn peer(&self) -> PeerAddr {
        PeerAddr::Inet(self.peer_addr)
    }

    async fn send(&mut self, pkt: &[u8]) -> Result<(), UbError> {
        self.socket.send_to(pkt, self.peer_addr).await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<BytesMut, UbError> {
        let mut rx = self.inbound_rx.lock().await;
        match rx.recv().await {
            Some(pkt) => Ok(pkt),
            None => Err(UbError::LinkDown),
        }
    }
}

#[async_trait]
impl Fabric for UdpFabric {
    fn kind(&self) -> &'static str {
        "udp"
    }

    async fn listen(&self, _local: PeerAddr) -> Result<Box<dyn Listener>, UbError> {
        Ok(Box::new(UdpListener {
            demux: Arc::clone(&self.demux),
            pending: Arc::clone(&self.pending),
            socket: Arc::clone(&self.socket),
            new_peer_rx: Arc::clone(&self.new_peer_rx),
        }))
    }

    async fn dial(&self, peer: PeerAddr) -> Result<Box<dyn Session>, UbError> {
        match peer {
            PeerAddr::Inet(addr) => {
                let (tx, rx) = mpsc::channel(UDP_CHANNEL_SIZE);
                self.demux.insert(addr, tx);
                Ok(Box::new(UdpSession {
                    socket: Arc::clone(&self.socket),
                    peer_addr: addr,
                    inbound_rx: Arc::new(Mutex::new(rx)),
                }))
            }
            PeerAddr::Unix(_) => Err(UbError::Internal("UDP fabric does not support Unix sockets".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_udp_fabric_kind() {
        let fabric = UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        assert_eq!(fabric.kind(), "udp");
    }

    #[tokio::test]
    async fn test_udp_fabric_send_recv() {
        let fabric_a = UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let fabric_b = UdpFabric::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();

        let addr_a = fabric_a.local_addr();
        let addr_b = fabric_b.local_addr();

        // B dials A
        let mut session_b = fabric_b.dial(PeerAddr::Inet(addr_a)).await.unwrap();

        // A listens
        let mut listener_a = fabric_a.listen(PeerAddr::Inet(addr_a)).await.unwrap();

        // B sends to A
        session_b.send(&[1, 2, 3]).await.unwrap();

        // A accepts the new peer session
        let mut session_a = listener_a.accept().await.unwrap();

        // A receives
        let pkt = session_a.recv().await.unwrap();
        assert_eq!(&pkt[..], &[1, 2, 3]);
    }
}
