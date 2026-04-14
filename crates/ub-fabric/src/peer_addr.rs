use std::net::SocketAddr;
use std::path::PathBuf;

/// Peer address abstraction supporting UDP/TCP and Unix domain sockets.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub enum PeerAddr {
    Inet(SocketAddr),
    Unix(PathBuf),
}

impl PeerAddr {
    pub fn inet(addr: SocketAddr) -> Self {
        PeerAddr::Inet(addr)
    }
}

impl std::fmt::Display for PeerAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerAddr::Inet(a) => write!(f, "{a}"),
            PeerAddr::Unix(p) => write!(f, "unix:{}", p.display()),
        }
    }
}

impl From<SocketAddr> for PeerAddr {
    fn from(addr: SocketAddr) -> Self {
        PeerAddr::Inet(addr)
    }
}
