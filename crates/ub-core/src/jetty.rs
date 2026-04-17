use std::collections::VecDeque;
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::addr::UbAddr;
use crate::config::JettyConfig;
use crate::error::{UbError, UB_ERR_FLUSHED};
use crate::types::{Cqe, JettyAddr, JettyHandle, Verb, WrId};

/// Pre-posted receive buffer in JFR.
#[derive(Debug)]
pub struct RecvBuffer {
    pub buf: Vec<u8>,
    pub wr_id: WrId,
}

/// Work request for pending send operation.
#[derive(Debug)]
pub struct WorkRequest {
    pub wr_id: WrId,
    pub dst_jetty: JettyAddr,
    pub data: Vec<u8>,
    pub imm: Option<u64>,
    pub verb: Verb,
    /// Target UB address for WriteImm; None for Send.
    pub ub_addr: Option<UbAddr>,
    /// Remote MR handle for WriteImm; None for Send.
    pub mr_handle: Option<u32>,
}

/// Jetty — connectionless send/recv unit (§8).
///
/// A Jetty contains three internal queues:
/// - **JFS** (Jetty Send Queue): MPSC, app tasks post WRs here
/// - **JFR** (Jetty Recv Queue): pre-posted receive buffers
/// - **JFC** (Jetty Completion Queue): completion events for both send and recv
pub struct Jetty {
    pub handle: JettyHandle,
    pub node_id: u16,
    /// JFS — send queue (MPSC: multiple producers post WRs, single consumer drains).
    jfs: Mutex<VecDeque<WorkRequest>>,
    /// JFR — receive queue (pre-posted buffers).
    jfr: Mutex<VecDeque<RecvBuffer>>,
    /// JFC — completion queue.
    jfc: Mutex<VecDeque<Cqe>>,
    /// Configuration limits.
    pub jfs_depth: u32,
    pub jfr_depth: u32,
    pub jfc_depth: u32,
    pub jfc_high_watermark: u32,
    /// Notification for JFS worker task.
    notify: Notify,
    /// Whether the Jetty has been closed.
    closed: Mutex<bool>,
}

impl Jetty {
    pub fn new(handle: JettyHandle, node_id: u16, config: &JettyConfig) -> Self {
        Jetty {
            handle,
            node_id,
            jfs: Mutex::new(VecDeque::new()),
            jfr: Mutex::new(VecDeque::new()),
            jfc: Mutex::new(VecDeque::new()),
            jfs_depth: config.jfs_depth,
            jfr_depth: config.jfr_depth,
            jfc_depth: config.jfc_depth,
            jfc_high_watermark: config.jfc_high_watermark,
            notify: Notify::new(),
            closed: Mutex::new(false),
        }
    }

    /// Post a receive buffer into the JFR.
    /// The buffer will be matched against an incoming Send message.
    pub fn post_recv(&self, buf: Vec<u8>, wr_id: WrId) -> Result<(), UbError> {
        let mut jfr = self.jfr.lock();
        if jfr.len() >= self.jfr_depth as usize {
            return Err(UbError::NoResources);
        }
        jfr.push_back(RecvBuffer { buf, wr_id });
        Ok(())
    }

    /// Post a work request (send) into the JFS.
    /// Returns NoResources if JFS is full or JFC is at high watermark.
    pub fn post_send(&self, wr: WorkRequest) -> Result<(), UbError> {
        // Check JFC backpressure first (FR-JETTY-3)
        {
            let jfc = self.jfc.lock();
            if jfc.len() >= self.jfc_high_watermark as usize {
                return Err(UbError::NoResources);
            }
        }

        let mut jfs = self.jfs.lock();
        if jfs.len() >= self.jfs_depth as usize {
            return Err(UbError::NoResources);
        }
        jfs.push_back(wr);
        self.notify.notify_one();
        Ok(())
    }

    /// Pop a work request from the JFS (for the worker task to process).
    pub fn pop_send(&self) -> Option<WorkRequest> {
        let mut jfs = self.jfs.lock();
        jfs.pop_front()
    }

    /// Get a notification permit for async waiting on the JFS.
    pub fn notify(&self) -> &Notify {
        &self.notify
    }

    /// Pop a receive buffer from the JFR (for matching against an incoming Send).
    pub fn pop_recv(&self) -> Option<RecvBuffer> {
        let mut jfr = self.jfr.lock();
        jfr.pop_front()
    }

    /// Push a completion queue entry onto the JFC.
    pub fn push_cqe(&self, cqe: Cqe) -> Result<(), UbError> {
        let mut jfc = self.jfc.lock();
        if jfc.len() >= self.jfc_depth as usize {
            return Err(UbError::NoResources);
        }
        jfc.push_back(cqe);
        Ok(())
    }

    /// Poll a CQE from the JFC (non-blocking).
    pub fn poll_cqe(&self) -> Option<Cqe> {
        let mut jfc = self.jfc.lock();
        jfc.pop_front()
    }

    /// Peek at the number of pending CQEs (for backpressure checks).
    pub fn cqe_count(&self) -> usize {
        self.jfc.lock().len()
    }

    /// Get the JettyAddr for this Jetty.
    pub fn addr(&self) -> JettyAddr {
        JettyAddr {
            node_id: self.node_id,
            jetty_id: self.handle.0,
        }
    }

    /// Close the Jetty. Flush all pending WRs with CQE(status=FLUSHED).
    pub fn close(&self) {
        let mut closed = self.closed.lock();
        if *closed {
            return;
        }
        *closed = true;

        // Flush JFS — generate FLUSHED CQEs for all pending WRs
        let mut jfs = self.jfs.lock();
        let mut jfc = self.jfc.lock();
        while let Some(wr) = jfs.pop_front() {
            let cqe = Cqe {
                wr_id: wr.wr_id,
                status: UB_ERR_FLUSHED,
                imm: None,
                byte_len: 0,
                jetty_id: self.handle.0,
                verb: wr.verb,
            };
            jfc.push_back(cqe);
        }

        // Flush JFR — generate FLUSHED CQEs for all posted recv buffers
        let mut jfr = self.jfr.lock();
        while let Some(recv_buf) = jfr.pop_front() {
            let cqe = Cqe {
                wr_id: recv_buf.wr_id,
                status: UB_ERR_FLUSHED,
                imm: None,
                byte_len: 0,
                jetty_id: self.handle.0,
                verb: Verb::Send,
            };
            jfc.push_back(cqe);
        }
    }

    /// Check if the Jetty is closed.
    pub fn is_closed(&self) -> bool {
        *self.closed.lock()
    }
}

/// Global Jetty table — maps JettyHandle → Arc<Jetty>.
pub struct JettyTable {
    entries: DashMap<u32, Arc<Jetty>>,
    next_handle: std::sync::atomic::AtomicU32,
    node_id: u16,
    config: JettyConfig,
    /// The first Jetty created becomes the default Jetty.
    default_jetty: std::sync::atomic::AtomicU32,
}

impl JettyTable {
    pub fn new(node_id: u16, config: JettyConfig) -> Self {
        JettyTable {
            entries: DashMap::new(),
            next_handle: std::sync::atomic::AtomicU32::new(1),
            node_id,
            config,
            default_jetty: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Create a new Jetty and register it in the table.
    /// The first Jetty created becomes the default Jetty.
    /// Returns the JettyHandle.
    pub fn create(&self) -> Result<JettyHandle, UbError> {
        let handle_val = self.next_handle.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let handle = JettyHandle(handle_val);
        let jetty = Arc::new(Jetty::new(handle, self.node_id, &self.config));
        self.entries.insert(handle_val, jetty);

        // Set as default if this is the first Jetty
        self.default_jetty.compare_exchange(
            0,
            handle_val,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ).ok(); // Ignore result — if already set, that's fine

        Ok(handle)
    }

    /// Get the default Jetty handle (the first Jetty created).
    /// Returns None if no Jetty has been created or the default was deregistered.
    pub fn default_jetty(&self) -> Option<JettyHandle> {
        let h = self.default_jetty.load(std::sync::atomic::Ordering::Acquire);
        if h == 0 {
            None
        } else {
            // Verify the Jetty still exists
            if self.entries.contains_key(&h) {
                Some(JettyHandle(h))
            } else {
                None
            }
        }
    }

    /// Look up a Jetty by handle.
    pub fn lookup(&self, handle: u32) -> Option<Arc<Jetty>> {
        self.entries.get(&handle).map(|r| Arc::clone(r.value()))
    }

    /// Deregister (close) a Jetty by handle.
    /// If this was the default Jetty, clears the default.
    pub fn deregister(&self, handle: u32) -> Result<(), UbError> {
        if let Some((_, jetty)) = self.entries.remove(&handle) {
            // Clear default if this was it
            self.default_jetty.compare_exchange(
                handle,
                0,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            ).ok();
            jetty.close();
            Ok(())
        } else {
            Err(UbError::AddrInvalid)
        }
    }

    /// List all registered Jettys.
    pub fn list(&self) -> Vec<JettyInfo> {
        self.entries.iter().map(|r| {
            let jetty = r.value();
            JettyInfo {
                handle: jetty.handle,
                node_id: jetty.node_id,
                jfs_depth: jetty.jfs_depth,
                jfr_depth: jetty.jfr_depth,
                jfc_depth: jetty.jfc_depth,
                jfc_high_watermark: jetty.jfc_high_watermark,
                cqe_count: jetty.cqe_count(),
                jfs_count: jetty.jfs.lock().len(),
                jfr_count: jetty.jfr.lock().len(),
            }
        }).collect()
    }
}

/// Summary info for a Jetty (for admin API).
#[derive(Debug, Clone, serde::Serialize)]
pub struct JettyInfo {
    pub handle: JettyHandle,
    pub node_id: u16,
    pub jfs_depth: u32,
    pub jfr_depth: u32,
    pub jfc_depth: u32,
    pub jfc_high_watermark: u32,
    pub cqe_count: usize,
    pub jfs_count: usize,
    pub jfr_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::UB_OK;

    fn default_config() -> JettyConfig {
        JettyConfig {
            jfs_depth: 16,
            jfr_depth: 16,
            jfc_depth: 16,
            jfc_high_watermark: 12,
        }
    }

    #[test]
    fn test_jetty_post_recv_and_pop() {
        let config = default_config();
        let jetty = Jetty::new(JettyHandle(1), 1, &config);

        jetty.post_recv(vec![0u8; 64], 100).unwrap();
        jetty.post_recv(vec![0u8; 128], 101).unwrap();
        jetty.post_recv(vec![0u8; 256], 102).unwrap();

        let buf1 = jetty.pop_recv().unwrap();
        assert_eq!(buf1.wr_id, 100);
        assert_eq!(buf1.buf.len(), 64);

        let buf2 = jetty.pop_recv().unwrap();
        assert_eq!(buf2.wr_id, 101);
        assert_eq!(buf2.buf.len(), 128);

        let buf3 = jetty.pop_recv().unwrap();
        assert_eq!(buf3.wr_id, 102);
    }

    #[test]
    fn test_jetty_post_send_and_pop() {
        let config = default_config();
        let jetty = Jetty::new(JettyHandle(1), 1, &config);

        let wr1 = WorkRequest {
            wr_id: 200,
            dst_jetty: JettyAddr { node_id: 2, jetty_id: 1 },
            data: vec![1, 2, 3],
            imm: None,
            verb: Verb::Send,
            ub_addr: None,
            mr_handle: None,
        };
        let wr2 = WorkRequest {
            wr_id: 201,
            dst_jetty: JettyAddr { node_id: 2, jetty_id: 1 },
            data: vec![4, 5, 6],
            imm: Some(42),
            verb: Verb::Send,
            ub_addr: None,
            mr_handle: None,
        };

        jetty.post_send(wr1).unwrap();
        jetty.post_send(wr2).unwrap();

        let popped1 = jetty.pop_send().unwrap();
        assert_eq!(popped1.wr_id, 200);

        let popped2 = jetty.pop_send().unwrap();
        assert_eq!(popped2.wr_id, 201);
        assert_eq!(popped2.imm, Some(42));
    }

    #[test]
    fn test_jetty_cqe_push_and_poll() {
        let config = default_config();
        let jetty = Jetty::new(JettyHandle(1), 1, &config);

        let cqe1 = Cqe {
            wr_id: 300,
            status: UB_OK,
            imm: None,
            byte_len: 3,
            jetty_id: 1,
            verb: Verb::Send,
        };
        let cqe2 = Cqe {
            wr_id: 301,
            status: UB_OK,
            imm: Some(99),
            byte_len: 5,
            jetty_id: 1,
            verb: Verb::Send,
        };

        jetty.push_cqe(cqe1).unwrap();
        jetty.push_cqe(cqe2).unwrap();

        let polled1 = jetty.poll_cqe().unwrap();
        assert_eq!(polled1.wr_id, 300);
        assert_eq!(polled1.imm, None);

        let polled2 = jetty.poll_cqe().unwrap();
        assert_eq!(polled2.wr_id, 301);
        assert_eq!(polled2.imm, Some(99));

        assert!(jetty.poll_cqe().is_none());
    }

    #[test]
    fn test_jetty_jfc_high_watermark_backpressure() {
        let config = default_config();
        let jetty = Jetty::new(JettyHandle(1), 1, &config);
        // jfc_high_watermark = 12

        // Fill JFC to the watermark
        for i in 0..12 {
            let cqe = Cqe {
                wr_id: i,
                status: UB_OK,
                imm: None,
                byte_len: 0,
                jetty_id: 1,
                verb: Verb::Send,
            };
            jetty.push_cqe(cqe).unwrap();
        }

        // Now post_send should fail with NoResources
        let wr = WorkRequest {
            wr_id: 999,
            dst_jetty: JettyAddr { node_id: 2, jetty_id: 1 },
            data: vec![1],
            imm: None,
            verb: Verb::Send,
            ub_addr: None,
            mr_handle: None,
        };
        assert!(matches!(jetty.post_send(wr), Err(UbError::NoResources)));

        // Poll a CQE to free space
        let _ = jetty.poll_cqe().unwrap();

        // Now post_send should succeed
        let wr2 = WorkRequest {
            wr_id: 1000,
            dst_jetty: JettyAddr { node_id: 2, jetty_id: 1 },
            data: vec![2],
            imm: None,
            verb: Verb::Send,
            ub_addr: None,
            mr_handle: None,
        };
        assert!(jetty.post_send(wr2).is_ok());
    }

    #[test]
    fn test_jetty_close_flushes_pending() {
        let config = default_config();
        let jetty = Jetty::new(JettyHandle(1), 1, &config);

        // Post some send WRs
        for i in 0..3 {
            let wr = WorkRequest {
                wr_id: i,
                dst_jetty: JettyAddr { node_id: 2, jetty_id: 1 },
                data: vec![],
                imm: None,
                verb: Verb::Send,
                ub_addr: None,
                mr_handle: None,
            };
            jetty.post_send(wr).unwrap();
        }

        // Post some recv buffers
        for i in 100..103 {
            jetty.post_recv(vec![0u8; 64], i).unwrap();
        }

        jetty.close();

        // JFS and JFR should be empty
        assert!(jetty.pop_send().is_none());
        assert!(jetty.pop_recv().is_none());

        // JFC should have 6 FLUSHED CQEs (3 sends + 3 recvs)
        let mut flushed_count = 0;
        while let Some(cqe) = jetty.poll_cqe() {
            assert_eq!(cqe.status, UB_ERR_FLUSHED);
            flushed_count += 1;
        }
        assert_eq!(flushed_count, 6);
    }

    #[test]
    fn test_jetty_table_create_lookup_deregister() {
        let config = default_config();
        let table = JettyTable::new(1, config);

        let h1 = table.create().unwrap();
        let h2 = table.create().unwrap();
        assert_ne!(h1.0, h2.0);

        let j1 = table.lookup(h1.0).unwrap();
        assert_eq!(j1.handle.0, h1.0);

        let info = table.list();
        assert_eq!(info.len(), 2);

        table.deregister(h1.0).unwrap();
        assert!(table.lookup(h1.0).is_none());
        assert!(table.lookup(h2.0).is_some());

        // Deregister non-existent
        assert!(matches!(table.deregister(9999), Err(UbError::AddrInvalid)));
    }

    #[test]
    fn test_jetty_jfr_depth_limit() {
        let config = JettyConfig {
            jfs_depth: 16,
            jfr_depth: 2,  // very small
            jfc_depth: 16,
            jfc_high_watermark: 12,
        };
        let jetty = Jetty::new(JettyHandle(1), 1, &config);

        jetty.post_recv(vec![0u8; 64], 1).unwrap();
        jetty.post_recv(vec![0u8; 64], 2).unwrap();
        // Third should fail
        assert!(matches!(jetty.post_recv(vec![0u8; 64], 3), Err(UbError::NoResources)));
    }

    #[test]
    fn test_jetty_jfs_depth_limit() {
        let config = JettyConfig {
            jfs_depth: 2,  // very small
            jfr_depth: 16,
            jfc_depth: 16,
            jfc_high_watermark: 12,
        };
        let jetty = Jetty::new(JettyHandle(1), 1, &config);

        for i in 0..2 {
            let wr = WorkRequest {
                wr_id: i,
                dst_jetty: JettyAddr { node_id: 2, jetty_id: 1 },
                data: vec![],
                imm: None,
                verb: Verb::Send,
                ub_addr: None,
                mr_handle: None,
            };
            jetty.post_send(wr).unwrap();
        }
        // Third should fail
        let wr = WorkRequest {
            wr_id: 2,
            dst_jetty: JettyAddr { node_id: 2, jetty_id: 1 },
            data: vec![],
            imm: None,
            verb: Verb::Send,
            ub_addr: None,
            mr_handle: None,
        };
        assert!(matches!(jetty.post_send(wr), Err(UbError::NoResources)));
    }

    #[test]
    fn test_jetty_jfc_backpressure_recovery() {
        let config = default_config();
        // jfc_depth = 16, jfc_high_watermark = 12
        let jetty = Jetty::new(JettyHandle(1), 1, &config);

        // Fill JFC to the high watermark
        for i in 0..12 {
            let cqe = Cqe {
                wr_id: i,
                status: UB_OK,
                imm: None,
                byte_len: 0,
                jetty_id: 1,
                verb: Verb::Send,
            };
            jetty.push_cqe(cqe).unwrap();
        }

        // post_send should be blocked
        let wr = WorkRequest {
            wr_id: 999,
            dst_jetty: JettyAddr { node_id: 2, jetty_id: 1 },
            data: vec![],
            imm: None,
            verb: Verb::Send,
            ub_addr: None,
            mr_handle: None,
        };
        assert!(matches!(jetty.post_send(wr), Err(UbError::NoResources)));

        // Drain 1 CQE — now below high watermark
        let _ = jetty.poll_cqe().unwrap();

        // post_send should succeed now
        let wr2 = WorkRequest {
            wr_id: 1000,
            dst_jetty: JettyAddr { node_id: 2, jetty_id: 1 },
            data: vec![1],
            imm: None,
            verb: Verb::Send,
            ub_addr: None,
            mr_handle: None,
        };
        assert!(jetty.post_send(wr2).is_ok());
    }

    #[test]
    fn test_default_jetty_first_create() {
        let config = default_config();
        let table = JettyTable::new(1, config);

        // No default yet
        assert!(table.default_jetty().is_none());

        let h1 = table.create().unwrap();
        // First Jetty becomes default
        assert_eq!(table.default_jetty(), Some(h1));

        let h2 = table.create().unwrap();
        // Second create does NOT change default
        assert_eq!(table.default_jetty(), Some(h1));
        assert_ne!(h1.0, h2.0);
    }

    #[test]
    fn test_default_jetty_not_changed_on_second_create() {
        let config = default_config();
        let table = JettyTable::new(1, config);
        let h1 = table.create().unwrap();
        let _h2 = table.create().unwrap();
        let _h3 = table.create().unwrap();
        assert_eq!(table.default_jetty(), Some(h1));
    }

    #[test]
    fn test_default_jetty_close_clears() {
        let config = default_config();
        let table = JettyTable::new(1, config);
        let h1 = table.create().unwrap();
        assert_eq!(table.default_jetty(), Some(h1));

        table.deregister(h1.0).unwrap();
        // Default cleared since h1 was removed
        assert!(table.default_jetty().is_none());

        // Creating another after default was cleared makes it the new default
        let h2 = table.create().unwrap();
        assert_eq!(table.default_jetty(), Some(h2));
    }
}
