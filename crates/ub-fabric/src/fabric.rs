use async_trait::async_trait;
use bytes::BytesMut;
use ub_core::error::UbError;

use crate::peer_addr::PeerAddr;

/// Fabric trait — abstract transport (UDP/TCP/UDS), §12.
#[async_trait]
pub trait Fabric: Send + Sync {
    fn kind(&self) -> &'static str;

    /// Start listening on the local address. Returns a Listener.
    async fn listen(&self, local: PeerAddr) -> Result<Box<dyn Listener>, UbError>;

    /// Establish a session to a remote peer.
    async fn dial(&self, peer: PeerAddr) -> Result<Box<dyn Session>, UbError>;
}

/// Listener — accepts incoming sessions.
#[async_trait]
pub trait Listener: Send {
    async fn accept(&mut self) -> Result<Box<dyn Session>, UbError>;
}

/// Session — bidirectional packet exchange with a peer.
#[async_trait]
pub trait Session: Send {
    fn peer(&self) -> PeerAddr;

    async fn send(&mut self, pkt: &[u8]) -> Result<(), UbError>;

    async fn recv(&mut self) -> Result<BytesMut, UbError>;
}
