use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;

use crate::device::Device;
use crate::error::UbError;
use crate::types::DeviceKind;

/// CPU memory device — backing is a heap-allocated byte array.
/// device_id is always 0 (FR-DEV-4).
pub struct MemoryDevice {
    data: RwLock<Box<[u8]>>,
    capacity: u64,
}

impl MemoryDevice {
    pub fn new(size: usize) -> Self {
        let data = vec![0u8; size].into_boxed_slice();
        let capacity = size as u64;
        MemoryDevice {
            data: RwLock::new(data),
            capacity,
        }
    }

    fn check_bounds(capacity: u64, offset: u64, len: usize) -> Result<(), UbError> {
        let end = offset as usize + len;
        if offset as usize > capacity as usize || end > capacity as usize {
            return Err(UbError::AddrInvalid);
        }
        Ok(())
    }

    fn check_alignment(offset: u64) -> Result<(), UbError> {
        if offset % 8 != 0 {
            return Err(UbError::Alignment);
        }
        Ok(())
    }
}

impl Device for MemoryDevice {
    fn kind(&self) -> DeviceKind {
        DeviceKind::Memory
    }

    fn device_id(&self) -> u16 {
        0
    }

    fn capacity(&self) -> u64 {
        self.capacity
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), UbError> {
        Self::check_bounds(self.capacity, offset, buf.len())?;
        let data = self.data.read();
        let start = offset as usize;
        buf.copy_from_slice(&data[start..start + buf.len()]);
        Ok(())
    }

    fn write(&self, offset: u64, buf: &[u8]) -> Result<(), UbError> {
        Self::check_bounds(self.capacity, offset, buf.len())?;
        let mut data = self.data.write();
        let start = offset as usize;
        data[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn atomic_cas(&self, offset: u64, expect: u64, new: u64) -> Result<u64, UbError> {
        Self::check_alignment(offset)?;
        Self::check_bounds(self.capacity, offset, 8)?;
        let data = self.data.read();
        let ptr = &data[offset as usize] as *const u8 as *const AtomicU64;
        let atomic = unsafe { &*ptr };
        let old = atomic.compare_exchange(expect, new, Ordering::AcqRel, Ordering::Acquire);
        match old {
            Ok(v) => Ok(v),
            Err(v) => Ok(v), // CAS failed, return current value
        }
    }

    fn atomic_faa(&self, offset: u64, add: u64) -> Result<u64, UbError> {
        Self::check_alignment(offset)?;
        Self::check_bounds(self.capacity, offset, 8)?;
        let data = self.data.read();
        let ptr = &data[offset as usize] as *const u8 as *const AtomicU64;
        let atomic = unsafe { &*ptr };
        Ok(atomic.fetch_add(add, Ordering::AcqRel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: write a u64 value in native byte order for atomic tests.
    fn write_u64_native(dev: &MemoryDevice, offset: u64, val: u64) {
        dev.write(offset, &val.to_ne_bytes()).unwrap();
    }

    /// Helper: read a u64 value in native byte order.
    fn read_u64_native(dev: &MemoryDevice, offset: u64) -> u64 {
        let mut buf = [0u8; 8];
        dev.read(offset, &mut buf).unwrap();
        u64::from_ne_bytes(buf)
    }

    #[test]
    fn test_memory_device_read_write() {
        let dev = MemoryDevice::new(4096);
        dev.write(0, &[1, 2, 3, 4]).unwrap();
        let mut buf = [0u8; 4];
        dev.read(0, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn test_memory_device_out_of_bounds() {
        let dev = MemoryDevice::new(16);
        assert!(dev.read(0, &mut [0u8; 17]).is_err());
        assert!(dev.write(8, &[0u8; 9]).is_err());
    }

    #[test]
    fn test_memory_device_atomic_cas_success() {
        let dev = MemoryDevice::new(4096);
        write_u64_native(&dev, 0, 0);

        let old = dev.atomic_cas(0, 0, 42).unwrap();
        assert_eq!(old, 0);

        // Verify value was updated
        assert_eq!(read_u64_native(&dev, 0), 42);
    }

    #[test]
    fn test_memory_device_atomic_cas_failure() {
        let dev = MemoryDevice::new(4096);
        write_u64_native(&dev, 0, 42);

        let old = dev.atomic_cas(0, 0, 99).unwrap();
        assert_eq!(old, 42); // CAS failed, old value returned

        // Value should NOT have changed
        assert_eq!(read_u64_native(&dev, 0), 42);
    }

    #[test]
    fn test_memory_device_atomic_faa() {
        let dev = MemoryDevice::new(4096);
        write_u64_native(&dev, 0, 42);

        let old = dev.atomic_faa(0, 10).unwrap();
        assert_eq!(old, 42);

        assert_eq!(read_u64_native(&dev, 0), 52);
    }

    #[test]
    fn test_memory_device_atomic_alignment_error() {
        let dev = MemoryDevice::new(4096);
        assert!(dev.atomic_cas(1, 0, 42).is_err());
        assert!(dev.atomic_faa(3, 10).is_err());
    }

    #[test]
    fn test_memory_device_atomic_out_of_bounds() {
        let dev = MemoryDevice::new(8);
        assert!(dev.atomic_cas(8, 0, 1).is_err());
    }

    #[test]
    fn test_memory_device_kind_and_id() {
        let dev = MemoryDevice::new(1024);
        assert_eq!(dev.kind(), DeviceKind::Memory);
        assert_eq!(dev.device_id(), 0);
        assert_eq!(dev.capacity(), 1024);
    }
}
