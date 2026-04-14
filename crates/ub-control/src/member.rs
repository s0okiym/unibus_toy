use parking_lot::RwLock;
use serde::Serialize;
use tokio::sync::mpsc;

use ub_core::types::NodeState;

/// Information about a peer node.
#[derive(Debug, Clone, Serialize)]
pub struct NodeInfo {
    pub node_id: u16,
    pub state: NodeState,
    pub control_addr: String,
    pub data_addr: String,
    pub epoch: u32,
    pub initial_credits: u32,
    pub last_seen: Option<u64>, // unix millis
    #[serde(skip)]
    pub tx: Option<mpsc::Sender<Vec<u8>>>,
}

/// Member table — maintains SuperPod view.
pub struct MemberTable {
    local_node_id: u16,
    nodes: RwLock<Vec<NodeInfo>>,
}

impl MemberTable {
    pub fn new(local_node_id: u16) -> Self {
        MemberTable {
            local_node_id,
            nodes: RwLock::new(Vec::new()),
        }
    }

    /// Add or update a node entry.
    pub fn upsert(&self, info: NodeInfo) {
        let mut nodes = self.nodes.write();
        if let Some(existing) = nodes.iter_mut().find(|n: &&mut NodeInfo| n.node_id == info.node_id) {
            existing.state = info.state;
            existing.control_addr = info.control_addr;
            existing.data_addr = info.data_addr;
            existing.epoch = info.epoch;
            existing.initial_credits = info.initial_credits;
            existing.tx = info.tx;
        } else {
            nodes.push(info);
        }
    }

    /// Mark a node as down.
    pub fn mark_down(&self, node_id: u16) {
        let mut nodes = self.nodes.write();
        if let Some(n) = nodes.iter_mut().find(|n| n.node_id == node_id) {
            n.state = NodeState::Down;
        }
    }

    /// Mark a node as suspect.
    pub fn mark_suspect(&self, node_id: u16) {
        let mut nodes = self.nodes.write();
        if let Some(n) = nodes.iter_mut().find(|n| n.node_id == node_id) {
            if n.state == NodeState::Active {
                n.state = NodeState::Suspect;
            }
        }
    }

    /// Mark a node as active.
    pub fn mark_active(&self, node_id: u16) {
        let mut nodes = self.nodes.write();
        if let Some(n) = nodes.iter_mut().find(|n| n.node_id == node_id) {
            n.state = NodeState::Active;
        }
    }

    /// Update last_seen timestamp for a node.
    pub fn update_last_seen(&self, node_id: u16, ts: u64) {
        let mut nodes = self.nodes.write();
        if let Some(n) = nodes.iter_mut().find(|n| n.node_id == node_id) {
            n.last_seen = Some(ts);
        }
    }

    /// Get a send channel to a peer.
    pub fn get_peer_tx(&self, node_id: u16) -> Option<mpsc::Sender<Vec<u8>>> {
        let nodes = self.nodes.read();
        nodes.iter().find(|n| n.node_id == node_id).and_then(|n: &NodeInfo| n.tx.clone())
    }

    /// Get all active peer node IDs.
    pub fn active_peer_ids(&self) -> Vec<u16> {
        let nodes = self.nodes.read();
        nodes.iter()
            .filter(|n| n.node_id != self.local_node_id && n.state != NodeState::Down)
            .map(|n: &NodeInfo| n.node_id)
            .collect()
    }

    /// Get all node info (for admin API). Strips tx channels.
    pub fn list(&self) -> Vec<NodeInfo> {
        let nodes = self.nodes.read();
        nodes.iter().map(|n: &NodeInfo| {
            let mut info = n.clone();
            info.tx = None;
            info
        }).collect()
    }

    /// Get all active peer senders (for internal broadcast).
    pub fn active_peer_senders(&self) -> Vec<(u16, mpsc::Sender<Vec<u8>>)> {
        let nodes = self.nodes.read();
        nodes.iter()
            .filter(|n| n.node_id != self.local_node_id && n.state != NodeState::Down)
            .filter_map(|n| n.tx.clone().map(|tx| (n.node_id, tx)))
            .collect()
    }

    /// Get node info by ID.
    pub fn get(&self, node_id: u16) -> Option<NodeInfo> {
        let nodes = self.nodes.read();
        nodes.iter().find(|n| n.node_id == node_id).map(|n: &NodeInfo| {
            let mut info = n.clone();
            info.tx = None;
            info
        })
    }

    /// Get the local node ID.
    pub fn local_node_id(&self) -> u16 {
        self.local_node_id
    }

    /// Broadcast a control message to all active peers.
    /// Collects senders first, then sends without holding the lock.
    pub async fn broadcast(&self, data: Vec<u8>) {
        let senders: Vec<(u16, mpsc::Sender<Vec<u8>>)> = {
            let nodes = self.nodes.read();
            nodes.iter()
                .filter(|n| n.node_id != self.local_node_id && n.state == NodeState::Active)
                .filter_map(|n| n.tx.clone().map(|tx| (n.node_id, tx)))
                .collect()
        };
        for (node_id, tx) in senders {
            if tx.send(data.clone()).await.is_err() {
                tracing::warn!("failed to send control message to node {}", node_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_member_table_upsert() {
        let table = MemberTable::new(1);
        table.upsert(NodeInfo {
            node_id: 2,
            state: NodeState::Active,
            control_addr: "10.0.0.2:7900".to_string(),
            data_addr: "10.0.0.2:7901".to_string(),
            epoch: 100,
            initial_credits: 64,
            last_seen: None,
            tx: None,
        });
        let info = table.get(2).unwrap();
        assert_eq!(info.node_id, 2);
        assert_eq!(info.state, NodeState::Active);
    }

    #[test]
    fn test_member_table_mark_down() {
        let table = MemberTable::new(1);
        table.upsert(NodeInfo {
            node_id: 2,
            state: NodeState::Active,
            control_addr: "10.0.0.2:7900".to_string(),
            data_addr: "10.0.0.2:7901".to_string(),
            epoch: 100,
            initial_credits: 64,
            last_seen: None,
            tx: None,
        });
        table.mark_down(2);
        let info = table.get(2).unwrap();
        assert_eq!(info.state, NodeState::Down);
    }

    #[test]
    fn test_member_table_mark_suspect_then_active() {
        let table = MemberTable::new(1);
        table.upsert(NodeInfo {
            node_id: 2,
            state: NodeState::Active,
            control_addr: "".to_string(),
            data_addr: "".to_string(),
            epoch: 0,
            initial_credits: 0,
            last_seen: None,
            tx: None,
        });
        table.mark_suspect(2);
        assert_eq!(table.get(2).unwrap().state, NodeState::Suspect);
        table.mark_active(2);
        assert_eq!(table.get(2).unwrap().state, NodeState::Active);
    }

    #[test]
    fn test_member_table_active_peers() {
        let table = MemberTable::new(1);
        table.upsert(NodeInfo {
            node_id: 2, state: NodeState::Active, control_addr: "".to_string(),
            data_addr: "".to_string(), epoch: 0, initial_credits: 0, last_seen: None, tx: None,
        });
        table.upsert(NodeInfo {
            node_id: 3, state: NodeState::Down, control_addr: "".to_string(),
            data_addr: "".to_string(), epoch: 0, initial_credits: 0, last_seen: None, tx: None,
        });
        let peers = table.active_peer_ids();
        assert_eq!(peers, vec![2]);
    }
}
