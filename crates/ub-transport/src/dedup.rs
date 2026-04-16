/// Result of checking a sequence number against the dedup window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupResult {
    /// Sequence number has already been seen (below window or marked in bitset).
    AlreadySeen,
    /// Sequence number is exactly the next expected (in-order).
    InOrder,
    /// Sequence number is within the window but ahead of next expected (gap exists).
    OutOfOrder,
    /// Sequence number is beyond the window boundary.
    BeyondWindow,
}

/// Sliding window dedup bitset for at-most-once semantics.
///
/// Uses a 1024-bit bitset (16 × u64) to track which sequence numbers have been
/// received. The window slides forward as in-order sequences are consumed.
pub struct DedupWindow {
    /// Next expected in-order sequence number.
    next_expected: u64,
    /// Bitset tracking received seqs within the window.
    /// Bit i corresponds to seq = next_expected + i.
    seen: [u64; 16],
    /// Window size in bits (must be <= 1024).
    window_size: usize,
}

impl DedupWindow {
    /// Create a new DedupWindow starting at the given initial sequence number.
    pub fn new(initial_seq: u64) -> Self {
        DedupWindow {
            next_expected: initial_seq,
            seen: [0u64; 16],
            window_size: 1024,
        }
    }

    /// Create a DedupWindow with a custom window size (must be <= 1024).
    pub fn with_window_size(initial_seq: u64, window_size: usize) -> Self {
        assert!(window_size <= 1024, "window_size must be <= 1024");
        DedupWindow {
            next_expected: initial_seq,
            seen: [0u64; 16],
            window_size,
        }
    }

    /// Check the status of a sequence number without modifying state.
    pub fn check(&self, seq: u64) -> DedupResult {
        if seq < self.next_expected {
            return DedupResult::AlreadySeen;
        }
        if seq == self.next_expected {
            return DedupResult::InOrder;
        }
        let offset = seq - self.next_expected;
        if offset >= self.window_size as u64 {
            return DedupResult::BeyondWindow;
        }
        if self.is_bit_set(offset as usize) {
            DedupResult::AlreadySeen
        } else {
            DedupResult::OutOfOrder
        }
    }

    /// Mark a sequence number as seen and advance next_expected.
    /// Returns the new next_expected value.
    pub fn mark_and_advance(&mut self, seq: u64) -> u64 {
        if seq < self.next_expected {
            // Already past this — no-op
            return self.next_expected;
        }

        let offset = seq - self.next_expected;
        if offset >= self.window_size as u64 {
            // Beyond window — cannot mark, but shouldn't normally happen
            return self.next_expected;
        }

        // Set the bit
        self.set_bit(offset as usize);

        // Advance next_expected while the leading bits are set
        while self.is_bit_set(0) {
            self.slide_one();
        }

        self.next_expected
    }

    /// Check if a sequence number within the window has been marked.
    pub fn is_seen(&self, seq: u64) -> bool {
        if seq < self.next_expected {
            return true;
        }
        let offset = seq - self.next_expected;
        if offset >= self.window_size as u64 {
            return false;
        }
        self.is_bit_set(offset as usize)
    }

    /// Reset the window (used on epoch change).
    pub fn reset(&mut self) {
        self.next_expected = 0;
        self.seen = [0u64; 16];
    }

    /// Reset with a specific initial sequence number.
    pub fn reset_to(&mut self, initial_seq: u64) {
        self.next_expected = initial_seq;
        self.seen = [0u64; 16];
    }

    /// Get the current next_expected value.
    pub fn next_expected(&self) -> u64 {
        self.next_expected
    }

    fn is_bit_set(&self, offset: usize) -> bool {
        if offset >= self.window_size {
            return false;
        }
        let word = offset / 64;
        let bit = offset % 64;
        (self.seen[word] >> bit) & 1 == 1
    }

    fn set_bit(&mut self, offset: usize) {
        if offset >= self.window_size {
            return;
        }
        let word = offset / 64;
        let bit = offset % 64;
        self.seen[word] |= 1u64 << bit;
    }

    /// Slide the window by one: shift all bits left by 1 and increment next_expected.
    fn slide_one(&mut self) {
        self.next_expected += 1;

        // Shift the 1024-bit bitset left by 1
        // word[0] gets word[0] >> 1 | (word[1] << 63), etc.
        for i in 0..16 {
            let low_bit_of_next = if i + 1 < 16 {
                (self.seen[i + 1] & 1) << 63
            } else {
                0
            };
            self.seen[i] = (self.seen[i] >> 1) | low_bit_of_next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_in_order_reception() {
        let mut dw = DedupWindow::new(0);
        assert_eq!(dw.check(0), DedupResult::InOrder);
        let next = dw.mark_and_advance(0);
        assert_eq!(next, 1);
        assert_eq!(dw.check(1), DedupResult::InOrder);
        dw.mark_and_advance(1);
        assert_eq!(dw.next_expected(), 2);
    }

    #[test]
    fn test_duplicate_detection() {
        let mut dw = DedupWindow::new(0);
        dw.mark_and_advance(0);
        assert_eq!(dw.check(0), DedupResult::AlreadySeen);

        dw.mark_and_advance(1);
        assert_eq!(dw.check(0), DedupResult::AlreadySeen);
        assert_eq!(dw.check(1), DedupResult::AlreadySeen);
    }

    #[test]
    fn test_out_of_order() {
        let mut dw = DedupWindow::new(0);
        // Receive seq=5 before seq=0..4
        assert_eq!(dw.check(5), DedupResult::OutOfOrder);
        dw.mark_and_advance(5);
        // seq=0 is still InOrder (not yet seen)
        assert_eq!(dw.check(0), DedupResult::InOrder);
        // seq=5 is now AlreadySeen within window
        assert_eq!(dw.check(5), DedupResult::AlreadySeen);
    }

    #[test]
    fn test_beyond_window() {
        let dw = DedupWindow::new(0);
        // seq=1024 is exactly at window boundary (0 + 1024)
        assert_eq!(dw.check(1024), DedupResult::BeyondWindow);
        // seq=2000 is far beyond
        assert_eq!(dw.check(2000), DedupResult::BeyondWindow);
    }

    #[test]
    fn test_window_slide_old_seqs_already_seen() {
        let mut dw = DedupWindow::new(0);
        // Receive 0..1023 in order to fill the window
        for seq in 0..1024u64 {
            assert_eq!(dw.check(seq), DedupResult::InOrder);
            dw.mark_and_advance(seq);
        }
        // next_expected should be 1024
        assert_eq!(dw.next_expected(), 1024);

        // Now seq=0 should be AlreadySeen (below window)
        assert_eq!(dw.check(0), DedupResult::AlreadySeen);
        // seq=1024 is now InOrder
        assert_eq!(dw.check(1024), DedupResult::InOrder);
    }

    #[test]
    fn test_gap_fill_advances_next_expected() {
        let mut dw = DedupWindow::new(0);
        // Receive seq=2 first (out of order)
        assert_eq!(dw.check(2), DedupResult::OutOfOrder);
        dw.mark_and_advance(2);
        // next_expected stays at 0 since bit 0 is not set
        assert_eq!(dw.next_expected(), 0);

        // Now receive seq=0
        assert_eq!(dw.check(0), DedupResult::InOrder);
        dw.mark_and_advance(0);
        // next_expected should advance to 1 (bit 0 was set, bit 1 not)
        assert_eq!(dw.next_expected(), 1);

        // Now receive seq=1
        dw.mark_and_advance(1);
        // next_expected should jump to 3 (bits 0,1,2 all set)
        assert_eq!(dw.next_expected(), 3);
    }

    #[test]
    fn test_reset() {
        let mut dw = DedupWindow::new(100);
        dw.mark_and_advance(100);
        dw.mark_and_advance(101);
        assert_eq!(dw.next_expected(), 102);

        dw.reset();
        assert_eq!(dw.next_expected(), 0);
        assert_eq!(dw.check(0), DedupResult::InOrder);
    }

    #[test]
    fn test_reset_to() {
        let mut dw = DedupWindow::new(0);
        dw.mark_and_advance(0);
        dw.mark_and_advance(1);
        dw.reset_to(1000);
        assert_eq!(dw.next_expected(), 1000);
        assert_eq!(dw.check(1000), DedupResult::InOrder);
    }

    #[test]
    fn test_custom_window_size() {
        let dw = DedupWindow::with_window_size(0, 64);
        // seq=64 is beyond window
        assert_eq!(dw.check(64), DedupResult::BeyondWindow);
        // seq=63 is out of order
        assert_eq!(dw.check(63), DedupResult::OutOfOrder);
    }

    #[test]
    fn test_is_seen() {
        let mut dw = DedupWindow::new(0);
        assert!(!dw.is_seen(0));
        dw.mark_and_advance(0);
        assert!(dw.is_seen(0));
        assert!(!dw.is_seen(1));
        // Below window counts as seen
        assert!(dw.is_seen(0));
    }
}