pub mod memory;
pub mod npu;

use crate::error::UbError;
use crate::types::DeviceKind;

/// Device trait — unified abstraction for CPU memory and simulated NPU (FR-DEV).
pub trait Device: Send + Sync {
    fn kind(&self) -> DeviceKind;
    fn device_id(&self) -> u16;
    fn capacity(&self) -> u64;

    /// Read `buf.len()` bytes starting at `offset` from this device.
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), UbError>;

    /// Write `buf` bytes starting at `offset` into this device.
    fn write(&self, offset: u64, buf: &[u8]) -> Result<(), UbError>;

    /// 8-byte atomic compare-and-swap. Address must be 8-byte aligned.
    /// Returns the old value.
    fn atomic_cas(&self, offset: u64, expect: u64, new: u64) -> Result<u64, UbError>;

    /// 8-byte atomic fetch-and-add. Address must be 8-byte aligned.
    /// Returns the old value.
    fn atomic_faa(&self, offset: u64, add: u64) -> Result<u64, UbError>;
}
