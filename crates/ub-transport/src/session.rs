use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ub_core::error::UbError;
use ub_wire::frame::{AckPayload, FrameFlags};

use crate::dedup::{DedupResult, DedupWindow};
use crate::read_cache::ReadResponseCache;

/// Identifies a reliable session by (local_node, remote_node, epoch).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub local_node: u16,
    pub remote_node: u16,
    pub epoch: u32,
}

/// Session state — Active means operational, Dead means terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Active,
    Dead,
}

/// An entry in the retransmit queue.
#[derive(Debug)]
pub struct RetransmitEntry {
    pub seq: u64,
    pub frame: Vec<u8>,
    pub first_sent_at: Instant,
    pub rto_deadline: Instant,
    pub retry_count: u32,
    pub is_fire_and_forget: bool,
    pub opaque: u64,
}

/// What action the caller should take for a received frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveAction {
    /// New, in-order frame — deliver to application.
    Deliver,
    /// Duplicate frame — already seen, just ACK.
    Duplicate,
    /// Out-of-order frame — gap exists, ACK with SACK.
    OutOfOrder,
    /// Beyond the dedup window — discard.
    BeyondWindow,
}

/// Per-peer reliable session.
///
/// Manages send-side sequencing, retransmission, credit tracking, and
/// receive-side dedup, execution records, and read response caching.
pub struct ReliableSession {
    pub key: SessionKey,
    pub state: SessionState,

    // === Send side ===
    /// Next sequence number to assign.
    pub next_seq: u64,
    /// Available credits for sending.
    pub available_credits: u32,
    /// Retransmit queue — entries awaiting ACK.
    pub retransmit_queue: VecDeque<RetransmitEntry>,
    /// Base RTO in milliseconds.
    pub rto_base_ms: u64,
    /// Maximum number of retries before giving up.
    pub max_retries: u32,

    // === AIMD congestion control ===
    /// Congestion window (in frames).
    pub cwnd: u32,
    /// Slow-start threshold.
    pub ssthresh: u32,

    // === EWMA RTT estimation ===
    /// Smoothed RTT in milliseconds.
    pub srtt: f64,
    /// RTT variance in milliseconds.
    pub rttvar: f64,
    /// Dynamic RTO in milliseconds (srtt + 4*rttvar, clamped to [200, 60000]).
    pub rto: u64,

    // === SACK fast retransmit ===
    /// Duplicate ACK count — incremented when ACK doesn't advance ack_seq but has new SACK.
    pub dup_ack_count: u32,
    /// Last ack_seq seen (for duplicate ACK detection).
    pub last_ack_seq: u64,

    // === Receive side ===
    /// Dedup window for at-most-once semantics.
    pub dedup: DedupWindow,
    /// Execution records: (seq, executed) — tracks which write-verb frames have been executed.
    pub execution_records: VecDeque<(u64, bool)>,
    /// Read response cache for idempotent read responses.
    pub read_cache: ReadResponseCache,
    /// Credits to grant back to the sender in the next ACK.
    pub credits_to_grant: u32,
    /// Number of frames received since the last ACK was sent.
    pub frames_since_last_ack: u32,
}

impl ReliableSession {
    /// Create a new reliable session.
    pub fn new(
        local_node: u16,
        remote_node: u16,
        epoch: u32,
        rto_base_ms: u64,
        max_retries: u32,
        initial_credits: u32,
    ) -> Self {
        let rto = rto_base_ms.max(200).min(60000);
        ReliableSession {
            key: SessionKey { local_node, remote_node, epoch },
            state: SessionState::Active,
            next_seq: 1, // Start at 1, 0 is reserved for "no seq"
            available_credits: initial_credits,
            retransmit_queue: VecDeque::new(),
            rto_base_ms,
            max_retries,
            cwnd: initial_credits,
            ssthresh: u32::MAX,
            srtt: rto_base_ms as f64,
            rttvar: rto_base_ms as f64 / 2.0,
            rto,
            dup_ack_count: 0,
            last_ack_seq: 0,
            dedup: DedupWindow::new(1),
            execution_records: VecDeque::new(),
            read_cache: ReadResponseCache::default_cache(),
            credits_to_grant: 0,
            frames_since_last_ack: 0,
        }
    }

    /// Assign a sequence number to a frame for sending.
    /// Modifies the frame bytes in-place to set stream_seq and ACK_REQ flag.
    /// Stores the frame in the retransmit queue.
    /// Returns the assigned sequence number.
    pub fn assign_seq(
        &mut self,
        frame: &mut [u8],
        is_fire_and_forget: bool,
        opaque: u64,
    ) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;

        // FrameHeader layout (32 bytes, big-endian):
        //   magic(4) + version(1) + frame_type(1) + flags(2) + src_node(2) +
        //   dst_node(2) + reserved(4) + stream_seq(8) + payload_len(4) + header_crc(4)
        //   offset 6-7: flags (u16)
        //   offset 16-23: stream_seq (u64)

        // Patch stream_seq
        if frame.len() >= 24 {
            frame[16..24].copy_from_slice(&seq.to_be_bytes());
        }

        // Set ACK_REQ flag in flags (offset 6, u16 BE)
        if frame.len() >= 8 {
            let flags = u16::from_be_bytes([frame[6], frame[7]]);
            let new_flags = flags | FrameFlags::ACK_REQ.bits();
            frame[6..8].copy_from_slice(&new_flags.to_be_bytes());
        }

        // Store in retransmit queue
        let rto = Duration::from_millis(self.rto);
        self.retransmit_queue.push_back(RetransmitEntry {
            seq,
            frame: frame.to_vec(),
            first_sent_at: Instant::now(),
            rto_deadline: Instant::now() + rto,
            retry_count: 0,
            is_fire_and_forget,
            opaque,
        });

        seq
    }

    /// Process an ACK payload — remove acknowledged fire-and-forget entries from the retransmit queue.
    /// Request-response entries are kept (they need retransmission until the response is received).
    /// Also updates RTT estimation (Karn algorithm) and AIMD state.
    /// Returns the list of removed entries (for any cleanup needed).
    pub fn process_ack(&mut self, ack: &AckPayload) -> Vec<RetransmitEntry> {
        let ack_seq = ack.ack_seq;
        let is_new_ack = ack_seq > self.last_ack_seq;

        // Remove fire-and-forget entries that have been ACKed.
        // Keep request-response entries — they may need retransmission if the response is lost.
        let mut removed = Vec::new();
        let mut keep = VecDeque::new();

        while let Some(entry) = self.retransmit_queue.pop_front() {
            if entry.seq <= ack_seq && entry.is_fire_and_forget {
                // RTT sample: only for non-retransmitted frames (Karn algorithm)
                if is_new_ack && entry.retry_count == 0 {
                    let sample_ms = entry.first_sent_at.elapsed().as_millis() as f64;
                    if sample_ms > 0.0 {
                        self.update_rtt(sample_ms);
                    }
                }
                removed.push(entry);
            } else {
                keep.push_back(entry);
            }
        }

        self.retransmit_queue = keep;

        // Add credits from the ACK
        self.add_credits(ack.credit_grant);

        // AIMD: update on new ACK
        if is_new_ack {
            self.on_new_ack();
        }

        removed
    }

    /// Remove a request-response entry by opaque — called when the response is received.
    /// Returns true if the entry was found and removed.
    pub fn complete_request(&mut self, opaque: u64) -> bool {
        if let Some(pos) = self.retransmit_queue.iter().position(|e| e.opaque == opaque && !e.is_fire_and_forget) {
            self.retransmit_queue.remove(pos);
            true
        } else {
            false
        }
    }

    /// Check a received frame's sequence number against the dedup window.
    /// Returns the appropriate ReceiveAction.
    pub fn receive_frame(&mut self, seq: u64) -> ReceiveAction {
        // Update frame counter
        self.frames_since_last_ack += 1;

        // Grant credit for this received frame
        self.credits_to_grant += 1;

        match self.dedup.check(seq) {
            DedupResult::InOrder => {
                self.dedup.mark_and_advance(seq);
                ReceiveAction::Deliver
            }
            DedupResult::AlreadySeen => ReceiveAction::Duplicate,
            DedupResult::OutOfOrder => {
                self.dedup.mark_and_advance(seq);
                ReceiveAction::OutOfOrder
            }
            DedupResult::BeyondWindow => ReceiveAction::BeyondWindow,
        }
    }

    /// Mark a write-verb frame as executed (for idempotency).
    pub fn mark_executed(&mut self, seq: u64) {
        // Find or add execution record
        if let Some(record) = self.execution_records.iter_mut().find(|(s, _)| *s == seq) {
            record.1 = true;
        } else {
            self.execution_records.push_back((seq, true));
            // Trim to window size
            while self.execution_records.len() > 1024 {
                self.execution_records.pop_front();
            }
        }
    }

    /// Check if a write-verb frame has already been executed.
    pub fn is_executed(&self, seq: u64) -> bool {
        self.execution_records.iter().any(|(s, executed)| *s == seq && *executed)
    }

    /// Add credits to the available pool.
    pub fn add_credits(&mut self, credits: u32) {
        self.available_credits = self.available_credits.saturating_add(credits);
    }

    /// Consume one credit for sending. Returns NoResources if none available.
    pub fn consume_credit(&mut self) -> Result<(), UbError> {
        if self.available_credits == 0 {
            return Err(UbError::NoResources);
        }
        self.available_credits -= 1;
        Ok(())
    }

    /// Check how many frames can be sent (congestion window minus outstanding).
    /// Returns 0 if congestion-limited.
    pub fn send_available(&self) -> u32 {
        let outstanding = self.retransmit_queue.len() as u32;
        self.cwnd.saturating_sub(outstanding)
    }

    /// Update RTT estimation with a new sample (Karn algorithm).
    /// Only called for non-retransmitted frames.
    pub fn update_rtt(&mut self, sample_ms: f64) {
        // EWMA: srtt = 0.875 * srtt + 0.125 * sample
        self.srtt = 0.875 * self.srtt + 0.125 * sample_ms;
        // rttvar = 0.75 * rttvar + 0.25 * |sample - srtt|
        self.rttvar = 0.75 * self.rttvar + 0.25 * (sample_ms - self.srtt).abs();
        // RTO = srtt + 4 * rttvar, clamped to [200ms, 60000ms]
        self.rto = (self.srtt + 4.0 * self.rttvar).round() as u64;
        self.rto = self.rto.max(200).min(60000);
    }

    /// AIMD: handle new ACK (ack_seq advanced).
    /// In slow start, cwnd doubles. In congestion avoidance, cwnd grows linearly.
    pub fn on_new_ack(&mut self) {
        if self.cwnd < self.ssthresh {
            // Slow start: exponential growth
            self.cwnd = self.cwnd.saturating_mul(2);
        } else {
            // Congestion avoidance: additive increase (1 MSS per RTT)
            self.cwnd += 1;
        }
        // Reset dup ACK count
        self.dup_ack_count = 0;
    }

    /// AIMD: handle packet loss (timeout or 3 dup ACKs).
    /// Multiplicative decrease: ssthresh = cwnd/2, cwnd = 1.
    pub fn on_loss(&mut self) {
        self.ssthresh = self.cwnd / 2;
        if self.ssthresh < 2 {
            self.ssthresh = 2;
        }
        self.cwnd = 1;
        self.dup_ack_count = 0;
    }

    /// Process an ACK for SACK fast retransmit detection.
    /// Returns Some(seq) if 3 duplicate ACKs detected (fast retransmit needed).
    /// Returns None otherwise.
    pub fn process_ack_for_fast_retransmit(&mut self, ack: &AckPayload) -> Option<u64> {
        let ack_seq = ack.ack_seq;

        if ack_seq > self.last_ack_seq {
            // New ACK — ack_seq advanced
            self.last_ack_seq = ack_seq;
            self.dup_ack_count = 0;
            None
        } else if ack.sack_bitmap.is_some() {
            // Duplicate ACK with SACK — possible gap fill
            self.dup_ack_count += 1;
            if self.dup_ack_count >= 3 {
                self.dup_ack_count = 0; // Reset after triggering
                Some(ack_seq + 1) // Retransmit the first unacked frame
            } else {
                None
            }
        } else {
            // Duplicate ACK without SACK — just count
            self.dup_ack_count += 1;
            if self.dup_ack_count >= 3 {
                self.dup_ack_count = 0;
                Some(ack_seq + 1)
            } else {
                None
            }
        }
    }

    /// Kill the session — mark as Dead and return all unacknowledged entries.
    pub fn kill(&mut self) -> Vec<RetransmitEntry> {
        self.state = SessionState::Dead;
        self.retransmit_queue.drain(..).collect()
    }

    /// Build an ACK payload with the current ack_seq, SACK bitmap, and credit_grant.
    pub fn build_ack_payload(&mut self, has_gap: bool) -> AckPayload {
        let ack_seq = self.dedup.next_expected().saturating_sub(1);
        let credit_grant = self.credits_to_grant;
        self.credits_to_grant = 0;
        self.frames_since_last_ack = 0;

        let sack_bitmap = if has_gap {
            let mut bitmap = [0u8; 32]; // 256-bit SACK bitmap
            // Bit i in the SACK bitmap indicates that ack_seq + 1 + i has been received
            let base = ack_seq + 1;
            for i in 0..256u64 {
                let seq = base + i;
                if self.dedup.is_seen(seq) {
                    bitmap[i as usize / 8] |= 1u8 << (i as usize % 8);
                }
            }
            Some(bitmap)
        } else {
            None
        };

        AckPayload {
            ack_seq,
            credit_grant,
            reserved: 0,
            sack_bitmap,
        }
    }

    /// Check for retransmit timeouts. Returns a list of seq numbers that need retransmission.
    /// Updates retry counts and deadlines. Kills the session if max_retries exceeded.
    pub fn check_rto(&mut self, now: Instant) -> Vec<u64> {
        if self.state != SessionState::Active {
            return Vec::new();
        }

        let mut timeout_seqs = Vec::new();
        let mut should_kill = false;

        for entry in &mut self.retransmit_queue {
            if now >= entry.rto_deadline {
                entry.retry_count += 1;
                if entry.retry_count > self.max_retries {
                    should_kill = true;
                    break;
                }
                // Exponential backoff: RTO * 2^retry_count
                let backoff = self.rto_base_ms * (1u64 << entry.retry_count.min(10));
                entry.rto_deadline = now + Duration::from_millis(backoff);
                timeout_seqs.push(entry.seq);
            }
        }

        if should_kill {
            self.kill();
            return Vec::new(); // Session is dead, caller handles it
        }

        // AIMD: on RTO timeout, multiplicative decrease
        if !timeout_seqs.is_empty() {
            self.on_loss();
        }

        timeout_seqs
    }

    /// Get a retransmit entry by seq number (for resending).
    pub fn get_retransmit_entry(&self, seq: u64) -> Option<&RetransmitEntry> {
        self.retransmit_queue.iter().find(|e| e.seq == seq)
    }

    /// Get the number of pending retransmit entries.
    pub fn pending_count(&self) -> usize {
        self.retransmit_queue.len()
    }

    /// Whether an ACK should be sent now (every 8 frames or on duplicate/out-of-order).
    pub fn should_send_ack(&self) -> bool {
        self.frames_since_last_ack >= 8 || self.credits_to_grant >= 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_session() -> ReliableSession {
        ReliableSession::new(1, 2, 100, 200, 8, 64)
    }

    #[test]
    fn test_assign_seq_increments() {
        let mut session = make_session();
        let mut frame = vec![0u8; 64];
        // Set a valid magic to avoid confusing downstream code
        frame[0..4].copy_from_slice(&0x5542_5543u32.to_be_bytes());

        let seq1 = session.assign_seq(&mut frame, false, 0);
        let seq2 = session.assign_seq(&mut frame, false, 1);
        assert_eq!(seq1, 1);
        assert_eq!(seq2, 2);
        assert_eq!(session.next_seq, 3);
        assert_eq!(session.retransmit_queue.len(), 2);
    }

    #[test]
    fn test_process_ack_removes_entries() {
        let mut session = make_session();
        let mut frame = vec![0u8; 64];

        // Fire-and-forget entries are removed by ACK
        session.assign_seq(&mut frame, true, 0);  // seq=1
        session.assign_seq(&mut frame, true, 1);  // seq=2
        // Request-response entries are kept by ACK (until response received)
        session.assign_seq(&mut frame, false, 2); // seq=3

        assert_eq!(session.retransmit_queue.len(), 3);

        let ack = AckPayload {
            ack_seq: 2,
            credit_grant: 0,
            reserved: 0,
            sack_bitmap: None,
        };
        let removed = session.process_ack(&ack);
        assert_eq!(removed.len(), 2); // Only fire-and-forget entries removed
        assert_eq!(session.retransmit_queue.len(), 1); // Request-response entry kept
        assert_eq!(session.retransmit_queue.front().unwrap().seq, 3);
        assert_eq!(session.retransmit_queue.front().unwrap().opaque, 2);
    }

    #[test]
    fn test_complete_request_removes_entry() {
        let mut session = make_session();
        let mut frame = vec![0u8; 64];

        session.assign_seq(&mut frame, false, 42);

        assert_eq!(session.retransmit_queue.len(), 1);

        // ACK doesn't remove it
        let ack = AckPayload {
            ack_seq: 1,
            credit_grant: 0,
            reserved: 0,
            sack_bitmap: None,
        };
        session.process_ack(&ack);
        assert_eq!(session.retransmit_queue.len(), 1); // Still there

        // complete_request removes it
        assert!(session.complete_request(42));
        assert_eq!(session.retransmit_queue.len(), 0);
    }

    #[test]
    fn test_credit_consume_and_add() {
        let mut session = make_session();
        assert_eq!(session.available_credits, 64);

        session.consume_credit().unwrap();
        session.consume_credit().unwrap();
        assert_eq!(session.available_credits, 62);

        session.add_credits(10);
        assert_eq!(session.available_credits, 72);
    }

    #[test]
    fn test_credit_exhausted() {
        let mut session = ReliableSession::new(1, 2, 100, 200, 8, 2);
        session.consume_credit().unwrap();
        session.consume_credit().unwrap();
        assert!(matches!(session.consume_credit(), Err(UbError::NoResources)));
    }

    #[test]
    fn test_receive_frame_in_order() {
        let mut session = make_session();
        assert_eq!(session.receive_frame(1), ReceiveAction::Deliver);
        assert_eq!(session.receive_frame(2), ReceiveAction::Deliver);
    }

    #[test]
    fn test_receive_frame_duplicate() {
        let mut session = make_session();
        session.receive_frame(1);
        assert_eq!(session.receive_frame(1), ReceiveAction::Duplicate);
    }

    #[test]
    fn test_receive_frame_out_of_order() {
        let mut session = make_session();
        assert_eq!(session.receive_frame(5), ReceiveAction::OutOfOrder);
    }

    #[test]
    fn test_receive_frame_beyond_window() {
        let mut session = make_session();
        assert_eq!(session.receive_frame(1025), ReceiveAction::BeyondWindow);
    }

    #[test]
    fn test_mark_executed_idempotency() {
        let mut session = make_session();
        assert!(!session.is_executed(1));
        session.mark_executed(1);
        assert!(session.is_executed(1));
        // Mark again — should still be true
        session.mark_executed(1);
        assert!(session.is_executed(1));
        // Unexecuted seq
        assert!(!session.is_executed(2));
    }

    #[test]
    fn test_kill_returns_all_entries() {
        let mut session = make_session();
        let mut frame = vec![0u8; 64];
        session.assign_seq(&mut frame, false, 0);
        session.assign_seq(&mut frame, false, 1);

        let killed = session.kill();
        assert_eq!(killed.len(), 2);
        assert_eq!(session.state, SessionState::Dead);
        assert_eq!(session.retransmit_queue.len(), 0);
    }

    #[test]
    fn test_build_ack_payload() {
        let mut session = make_session();
        session.receive_frame(1);
        session.receive_frame(2);

        let ack = session.build_ack_payload(false);
        // ack_seq = next_expected - 1 = 3 - 1 = 2
        assert_eq!(ack.ack_seq, 2);
        // credits_to_grant = 2 (received 2 frames)
        assert_eq!(ack.credit_grant, 2);
        // frames_since_last_ack should be reset
        assert_eq!(session.frames_since_last_ack, 0);
    }

    #[test]
    fn test_build_ack_payload_with_sack() {
        let mut session = make_session();
        // Receive seq=1 and seq=3 (gap at seq=2)
        session.receive_frame(1);
        session.receive_frame(3);

        let ack = session.build_ack_payload(true);
        // ack_seq = next_expected - 1 = 1 - 1 = 0 (gap at seq=2, next_expected still 1)
        // Wait — actually dedup starts at 1, and after receiving 1:
        // - dedup.next_expected = 2
        // Then receiving 3: OutOfOrder, mark_and_advance(3), but next_expected stays 2
        // So ack_seq = 2 - 1 = 1
        assert_eq!(ack.ack_seq, 1);
        // SACK bitmap: bit for (ack_seq+1+0)=2 should NOT be set (not received)
        // bit for (ack_seq+1+1)=3 should be set (received)
        let bitmap = ack.sack_bitmap.unwrap();
        assert_eq!(bitmap[0] & 0b10, 0b10); // bit 1 set (seq=3)
        assert_eq!(bitmap[0] & 0b01, 0);     // bit 0 not set (seq=2)
    }

    #[test]
    fn test_check_rto_detects_timeout() {
        let mut session = ReliableSession::new(1, 2, 100, 100, 8, 64);
        let mut frame = vec![0u8; 64];
        session.assign_seq(&mut frame, false, 0);

        // Not timed out yet
        let timeouts = session.check_rto(Instant::now());
        assert!(timeouts.is_empty());

        // After RTO
        let future = Instant::now() + Duration::from_millis(200);
        let timeouts = session.check_rto(future);
        assert_eq!(timeouts.len(), 1);
        assert_eq!(timeouts[0], 1);
    }

    #[test]
    fn test_check_rto_max_retries_kills_session() {
        let mut session = ReliableSession::new(1, 2, 100, 50, 2, 64);
        let mut frame = vec![0u8; 64];
        session.assign_seq(&mut frame, false, 0);

        // Simulate exceeding max retries
        let entry = session.retransmit_queue.front_mut().unwrap();
        entry.retry_count = 2; // max_retries = 2

        let future = Instant::now() + Duration::from_millis(200);
        let timeouts = session.check_rto(future);
        // Session should be killed
        assert!(timeouts.is_empty()); // Empty because session is dead
        assert_eq!(session.state, SessionState::Dead);
    }

    #[test]
    fn test_should_send_ack() {
        let mut session = make_session();
        assert!(!session.should_send_ack());
        for _ in 0..8 {
            session.frames_since_last_ack += 1;
        }
        assert!(session.should_send_ack());
    }

    #[test]
    fn test_aimd_slow_start() {
        let mut session = ReliableSession::new(1, 2, 100, 200, 8, 4);
        // cwnd starts at initial_credits = 4, ssthresh = MAX
        assert_eq!(session.cwnd, 4);

        // Slow start: cwnd doubles on each new ACK
        session.on_new_ack();
        assert_eq!(session.cwnd, 8); // 4 * 2

        session.on_new_ack();
        assert_eq!(session.cwnd, 16); // 8 * 2
    }

    #[test]
    fn test_aimd_congestion_avoidance() {
        let mut session = ReliableSession::new(1, 2, 100, 200, 8, 4);
        // Force into congestion avoidance
        session.ssthresh = 8;
        session.cwnd = 8;

        // Congestion avoidance: cwnd += 1 (additive increase)
        session.on_new_ack();
        assert_eq!(session.cwnd, 9);

        session.on_new_ack();
        assert_eq!(session.cwnd, 10);
    }

    #[test]
    fn test_aimd_multiplicative_decrease() {
        let mut session = ReliableSession::new(1, 2, 100, 200, 8, 64);
        session.cwnd = 32;

        // On loss: ssthresh = cwnd/2, cwnd = 1
        session.on_loss();
        assert_eq!(session.ssthresh, 16);
        assert_eq!(session.cwnd, 1);
    }

    #[test]
    fn test_rtt_estimation() {
        let mut session = ReliableSession::new(1, 2, 100, 200, 8, 64);
        // Initial: srtt=200, rttvar=100, rto=200 (clamped)

        // Sample 1: 50ms
        session.update_rtt(50.0);
        // srtt = 0.875*200 + 0.125*50 = 175 + 6.25 = 181.25
        let expected_srtt = 0.875 * 200.0 + 0.125 * 50.0;
        assert!((session.srtt - expected_srtt).abs() < 0.01);

        // RTO should be adaptive
        let expected_rto = session.srtt + 4.0 * session.rttvar;
        let clamped = expected_rto.round() as u64;
        assert_eq!(session.rto, clamped.max(200).min(60000));
    }

    #[test]
    fn test_rtt_karn_skip_retransmit() {
        let mut session = make_session();
        let mut frame = vec![0u8; 64];
        frame[0..4].copy_from_slice(&0x5542_5543u32.to_be_bytes());
        session.assign_seq(&mut frame, true, 0);

        // Simulate retransmission by incrementing retry_count
        session.retransmit_queue.front_mut().unwrap().retry_count = 1;

        let initial_srtt = session.srtt;

        // Process ACK for retransmitted frame — should NOT update RTT
        let ack = AckPayload {
            ack_seq: 1,
            credit_grant: 1,
            reserved: 0,
            sack_bitmap: None,
        };
        session.process_ack(&ack);

        // srtt should remain unchanged (Karn: skip retransmitted frame RTT samples)
        assert_eq!(session.srtt, initial_srtt);
    }

    #[test]
    fn test_sack_fast_retransmit() {
        let mut session = make_session();

        // First, set last_ack_seq by processing an initial ACK
        session.last_ack_seq = 1;

        // Simulate 3 duplicate ACKs with SACK
        let mut bitmap = [0u8; 32];
        bitmap[0] = 0b10; // seq=3 received

        let ack = AckPayload {
            ack_seq: 1, // Not advancing past last_ack_seq=1
            credit_grant: 0,
            reserved: 0,
            sack_bitmap: Some(bitmap),
        };

        // First dup ACK — no retransmit
        assert!(session.process_ack_for_fast_retransmit(&ack).is_none());
        // Second dup ACK — no retransmit
        assert!(session.process_ack_for_fast_retransmit(&ack).is_none());
        // Third dup ACK — trigger fast retransmit
        let result = session.process_ack_for_fast_retransmit(&ack);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), 2); // Retransmit seq=2 (ack_seq+1)
    }

    #[test]
    fn test_send_available() {
        let mut session = ReliableSession::new(1, 2, 100, 200, 8, 64);
        // cwnd=64, no outstanding frames
        assert_eq!(session.send_available(), 64);

        // Add some outstanding frames
        let mut frame = vec![0u8; 64];
        frame[0..4].copy_from_slice(&0x5542_5543u32.to_be_bytes());
        session.assign_seq(&mut frame, false, 0);
        session.assign_seq(&mut frame, false, 1);
        session.assign_seq(&mut frame, false, 2);

        // 64 - 3 = 61
        assert_eq!(session.send_available(), 61);
    }
}