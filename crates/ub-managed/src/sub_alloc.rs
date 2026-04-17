use std::sync::atomic::{AtomicU64, Ordering};

/// Bump allocator for sub-allocation within a pool MR.
///
/// Allocates offsets sequentially; free is a no-op (bump allocator does not reclaim).
/// Suitable for region-based allocation where regions live for the lifetime of the pool.
pub struct SubAllocator {
    base_offset: u64,
    total_len: u64,
    allocated: AtomicU64,
}

impl SubAllocator {
    pub fn new(base_offset: u64, total_len: u64) -> Self {
        SubAllocator {
            base_offset,
            total_len,
            allocated: AtomicU64::new(0),
        }
    }

    /// Allocate `size` bytes from the pool. Returns the absolute offset (base + alloc_offset).
    /// Returns `None` if insufficient space.
    pub fn alloc(&self, size: u64) -> Option<u64> {
        // CAS loop for lock-free bump allocation
        loop {
            let current = self.allocated.load(Ordering::Acquire);
            if current + size > self.total_len {
                return None;
            }
            if self.allocated.compare_exchange(current, current + size, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return Some(self.base_offset + current);
            }
        }
    }

    /// Free is a no-op for bump allocator.
    pub fn free(&self, _offset: u64, _size: u64) {
        // Bump allocator does not reclaim space
    }

    /// Current allocated bytes.
    pub fn allocated_bytes(&self) -> u64 {
        self.allocated.load(Ordering::Acquire)
    }

    /// Free bytes remaining.
    pub fn free_bytes(&self) -> u64 {
        self.total_len.saturating_sub(self.allocated.load(Ordering::Acquire))
    }

    pub fn base_offset(&self) -> u64 {
        self.base_offset
    }

    pub fn total_len(&self) -> u64 {
        self.total_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sub_allocator_bump() {
        let alloc = SubAllocator::new(1024, 4096);
        let off1 = alloc.alloc(512).unwrap();
        assert_eq!(off1, 1024); // base_offset + 0

        let off2 = alloc.alloc(256).unwrap();
        assert_eq!(off2, 1024 + 512); // base_offset + 512

        let off3 = alloc.alloc(1024).unwrap();
        assert_eq!(off3, 1024 + 512 + 256); // base_offset + 768

        assert_eq!(alloc.allocated_bytes(), 512 + 256 + 1024);
        assert_eq!(alloc.free_bytes(), 4096 - 512 - 256 - 1024);
    }

    #[test]
    fn test_sub_allocator_exhaustion() {
        let alloc = SubAllocator::new(0, 1024);
        assert!(alloc.alloc(512).is_some());
        assert!(alloc.alloc(512).is_some());
        assert!(alloc.alloc(1).is_none()); // exhausted
    }

    #[test]
    fn test_sub_allocator_exact_fit() {
        let alloc = SubAllocator::new(0, 100);
        assert!(alloc.alloc(100).is_some()); // exact fit
        assert!(alloc.alloc(1).is_none());
    }

    #[test]
    fn test_sub_allocator_free_is_noop() {
        let alloc = SubAllocator::new(0, 1024);
        let off = alloc.alloc(256).unwrap();
        alloc.free(off, 256); // no-op, space not reclaimed
        // Still exhausted after first alloc + no-op free + second alloc
        assert!(alloc.alloc(768).is_some());
        assert!(alloc.alloc(1).is_none());
    }
}