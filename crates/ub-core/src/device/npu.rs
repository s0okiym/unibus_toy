use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;

use crate::device::Device;
use crate::error::UbError;
use crate::types::{DeviceKind, StorageTier};

/// Simulated NPU device — backing is a RwLock<Vec<u8>> (FR-DEV-2).
/// device_id >= 1, auto-incremented per ub_npu_open call.
pub struct NpuDevice {
    device_id: u16,
    data: RwLock<Vec<u8>>,
    capacity: u64,
    alloc_cursor: RwLock<u64>,
}

impl NpuDevice {
    pub fn new(device_id: u16, mem_size_mib: u64) -> Self {
        let size = (mem_size_mib as usize) * 1024 * 1024;
        NpuDevice {
            device_id,
            data: RwLock::new(vec![0u8; size]),
            capacity: size as u64,
            alloc_cursor: RwLock::new(0),
        }
    }

    /// Allocate `len` bytes with `align` alignment from the NPU device.
    /// Returns (offset, len) on success. Bump allocator (FR-DEV-5).
    pub fn alloc(&self, len: u64, align: u64) -> Result<(u64, u64), UbError> {
        let mut cursor = self.alloc_cursor.write();
        // Align up
        let aligned = (*cursor + align - 1) & !(align - 1);
        let end = aligned + len;
        if end > self.capacity {
            return Err(UbError::NoResources);
        }
        *cursor = end;
        Ok((aligned, len))
    }

    fn check_bounds(&self, offset: u64, len: usize) -> Result<(), UbError> {
        let end = offset as usize + len;
        if offset as usize > self.capacity as usize || end > self.capacity as usize {
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

impl Device for NpuDevice {
    fn kind(&self) -> DeviceKind {
        DeviceKind::Npu
    }

    fn device_id(&self) -> u16 {
        self.device_id
    }

    fn capacity(&self) -> u64 {
        self.capacity
    }

    fn tier(&self) -> StorageTier {
        StorageTier::Hot
    }

    fn peak_read_bw_mbps(&self) -> u32 {
        50_000
    }

    fn peak_write_bw_mbps(&self) -> u32 {
        50_000
    }

    fn read_latency_ns_p50(&self) -> u32 {
        50
    }

    fn write_latency_ns_p50(&self) -> u32 {
        50
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), UbError> {
        self.check_bounds(offset, buf.len())?;
        let data = self.data.read();
        let start = offset as usize;
        buf.copy_from_slice(&data[start..start + buf.len()]);
        Ok(())
    }

    fn write(&self, offset: u64, buf: &[u8]) -> Result<(), UbError> {
        self.check_bounds(offset, buf.len())?;
        let mut data = self.data.write();
        let start = offset as usize;
        data[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn atomic_cas(&self, offset: u64, expect: u64, new: u64) -> Result<u64, UbError> {
        Self::check_alignment(offset)?;
        self.check_bounds(offset, 8)?;
        let data = self.data.read();
        let ptr = &data[offset as usize] as *const u8 as *const AtomicU64;
        let atomic = unsafe { &*ptr };
        let old = atomic.compare_exchange(expect, new, Ordering::AcqRel, Ordering::Acquire);
        match old {
            Ok(v) => Ok(v),
            Err(v) => Ok(v),
        }
    }

    fn atomic_faa(&self, offset: u64, add: u64) -> Result<u64, UbError> {
        Self::check_alignment(offset)?;
        self.check_bounds(offset, 8)?;
        let data = self.data.read();
        let ptr = &data[offset as usize] as *const u8 as *const AtomicU64;
        let atomic = unsafe { &*ptr };
        Ok(atomic.fetch_add(add, Ordering::AcqRel))
    }
}

/// Global NPU device ID allocator.
static NEXT_NPU_DEVICE_ID: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(1);

/// Open a simulated NPU device with the given memory size.
pub fn ub_npu_open(mem_size_mib: u64) -> std::sync::Arc<NpuDevice> {
    let device_id = NEXT_NPU_DEVICE_ID.fetch_add(1, Ordering::Relaxed);
    std::sync::Arc::new(NpuDevice::new(device_id, mem_size_mib))
}

/// Allocate from an NPU device. Convenience wrapper.
pub fn ub_npu_alloc(npu: &NpuDevice, len: u64, align: u64) -> Result<(u64, u64), UbError> {
    npu.alloc(len, align)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_npu_device_read_write() {
        let dev = NpuDevice::new(1, 1); // 1 MiB
        dev.write(0, &[1, 2, 3, 4]).unwrap();
        let mut buf = [0u8; 4];
        dev.read(0, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn test_npu_device_atomic_cas() {
        let dev = NpuDevice::new(1, 1);
        dev.write(0, &0u64.to_ne_bytes()).unwrap();
        let old = dev.atomic_cas(0, 0, 42).unwrap();
        assert_eq!(old, 0);
        let mut buf = [0u8; 8];
        dev.read(0, &mut buf).unwrap();
        assert_eq!(u64::from_ne_bytes(buf), 42);
    }

    #[test]
    fn test_npu_device_atomic_faa() {
        let dev = NpuDevice::new(1, 1);
        dev.write(0, &10u64.to_ne_bytes()).unwrap();
        let old = dev.atomic_faa(0, 5).unwrap();
        assert_eq!(old, 10);
        let mut buf = [0u8; 8];
        dev.read(0, &mut buf).unwrap();
        assert_eq!(u64::from_ne_bytes(buf), 15);
    }

    #[test]
    fn test_npu_alloc_no_overlap() {
        let dev = NpuDevice::new(1, 1);
        let (off1, _) = dev.alloc(1024, 8).unwrap();
        let (off2, _) = dev.alloc(1024, 8).unwrap();
        assert_eq!(off1, 0);
        assert_eq!(off2, 1024);
    }

    #[test]
    fn test_npu_alloc_alignment() {
        let dev = NpuDevice::new(1, 1);
        let (off, _) = dev.alloc(100, 64).unwrap();
        assert_eq!(off % 64, 0);
    }

    #[test]
    fn test_npu_device_kind_and_id() {
        let dev = NpuDevice::new(1, 1);
        assert_eq!(dev.kind(), DeviceKind::Npu);
        assert!(dev.device_id() >= 1);
    }

    #[test]
    fn test_npu_device_tier_is_hot() {
        let dev = NpuDevice::new(1, 1);
        assert_eq!(dev.tier(), StorageTier::Hot);
        assert_eq!(dev.peak_read_bw_mbps(), 50_000);
        assert_eq!(dev.read_latency_ns_p50(), 50);
    }

    #[test]
    fn test_ub_npu_open() {
        let npu = ub_npu_open(1);
        assert_eq!(npu.kind(), DeviceKind::Npu);
        assert!(npu.device_id() >= 1);
    }
}
