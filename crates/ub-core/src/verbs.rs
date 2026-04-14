use crate::addr::UbAddr;
use crate::error::UbError;
use crate::mr::MrTable;
use crate::types::Verb;

/// Synchronous local read: lookup MR, check READ permission, read from device.
pub fn ub_read_sync(mr_table: &MrTable, addr: UbAddr, buf: &mut [u8]) -> Result<(), UbError> {
    let (entry, offset_in_mr) = mr_table
        .lookup_by_addr(addr)
        .ok_or(UbError::AddrInvalid)?;
    entry.check_perms(Verb::ReadReq)?;
    let device_offset = entry.base_offset + offset_in_mr;
    entry.device.read(device_offset, buf)
}

/// Synchronous local write: lookup MR, check WRITE permission, write to device.
pub fn ub_write_sync(mr_table: &MrTable, addr: UbAddr, buf: &[u8]) -> Result<(), UbError> {
    let (entry, offset_in_mr) = mr_table
        .lookup_by_addr(addr)
        .ok_or(UbError::AddrInvalid)?;
    entry.check_perms(Verb::Write)?;
    let device_offset = entry.base_offset + offset_in_mr;
    entry.device.write(device_offset, buf)
}

/// Synchronous local atomic compare-and-swap: lookup MR, check ATOMIC permission,
/// verify alignment, then execute CAS on device.
pub fn ub_atomic_cas_sync(
    mr_table: &MrTable,
    addr: UbAddr,
    expect: u64,
    new: u64,
) -> Result<u64, UbError> {
    if addr.offset() % 8 != 0 {
        return Err(UbError::Alignment);
    }
    let (entry, offset_in_mr) = mr_table
        .lookup_by_addr(addr)
        .ok_or(UbError::AddrInvalid)?;
    entry.check_perms(Verb::AtomicCas)?;
    let device_offset = entry.base_offset + offset_in_mr;
    entry.device.atomic_cas(device_offset, expect, new)
}

/// Synchronous local atomic fetch-and-add: lookup MR, check ATOMIC permission,
/// verify alignment, then execute FAA on device.
pub fn ub_atomic_faa_sync(
    mr_table: &MrTable,
    addr: UbAddr,
    add: u64,
) -> Result<u64, UbError> {
    if addr.offset() % 8 != 0 {
        return Err(UbError::Alignment);
    }
    let (entry, offset_in_mr) = mr_table
        .lookup_by_addr(addr)
        .ok_or(UbError::AddrInvalid)?;
    entry.check_perms(Verb::AtomicFaa)?;
    let device_offset = entry.base_offset + offset_in_mr;
    entry.device.atomic_faa(device_offset, add)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::memory::MemoryDevice;
    use crate::types::MrPerms;
    use std::sync::Arc;

    fn make_table_with_mr(perms: MrPerms, len: u64) -> (MrTable, UbAddr) {
        let table = MrTable::new(1, 42);
        let dev = Arc::new(MemoryDevice::new(4096));
        let (addr, _handle) = table.register(dev, len, perms).unwrap();
        (table, addr)
    }

    #[test]
    fn test_ub_read_sync_ok() {
        let (table, addr) = make_table_with_mr(MrPerms::READ | MrPerms::WRITE, 1024);
        // Write some data first
        ub_write_sync(&table, addr, &[1, 2, 3, 4]).unwrap();
        let mut buf = [0u8; 4];
        ub_read_sync(&table, addr, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn test_ub_read_sync_perm_denied() {
        let (table, addr) = make_table_with_mr(MrPerms::WRITE, 1024);
        let mut buf = [0u8; 4];
        assert!(matches!(ub_read_sync(&table, addr, &mut buf), Err(UbError::PermDenied)));
    }

    #[test]
    fn test_ub_write_sync_perm_denied() {
        let (table, addr) = make_table_with_mr(MrPerms::READ, 1024);
        assert!(matches!(ub_write_sync(&table, addr, &[1, 2, 3]), Err(UbError::PermDenied)));
    }

    #[test]
    fn test_ub_atomic_cas_sync_ok() {
        let (table, addr) = make_table_with_mr(MrPerms::WRITE | MrPerms::ATOMIC, 1024);
        // Write initial value
        ub_write_sync(&table, addr, &0u64.to_ne_bytes()).unwrap();
        let old = ub_atomic_cas_sync(&table, addr, 0, 42).unwrap();
        assert_eq!(old, 0);
    }

    #[test]
    fn test_ub_atomic_cas_sync_perm_denied() {
        let (table, addr) = make_table_with_mr(MrPerms::READ | MrPerms::WRITE, 1024);
        assert!(matches!(
            ub_atomic_cas_sync(&table, addr, 0, 42),
            Err(UbError::PermDenied)
        ));
    }

    #[test]
    fn test_ub_atomic_cas_sync_alignment() {
        let (table, base_addr) = make_table_with_mr(MrPerms::ATOMIC, 1024);
        let misaligned = base_addr.with_offset(base_addr.offset() + 3);
        assert!(matches!(
            ub_atomic_cas_sync(&table, misaligned, 0, 42),
            Err(UbError::Alignment)
        ));
    }

    #[test]
    fn test_ub_atomic_faa_sync_ok() {
        let (table, addr) = make_table_with_mr(MrPerms::WRITE | MrPerms::ATOMIC, 1024);
        ub_write_sync(&table, addr, &10u64.to_ne_bytes()).unwrap();
        let old = ub_atomic_faa_sync(&table, addr, 5).unwrap();
        assert_eq!(old, 10);
    }

    #[test]
    fn test_ub_atomic_faa_sync_perm_denied() {
        let (table, addr) = make_table_with_mr(MrPerms::READ, 1024);
        assert!(matches!(
            ub_atomic_faa_sync(&table, addr, 5),
            Err(UbError::PermDenied)
        ));
    }

    #[test]
    fn test_ub_read_sync_addr_invalid() {
        let table = MrTable::new(1, 42);
        let bad_addr = UbAddr::new(99, 99, 99, 0, 0);
        let mut buf = [0u8; 4];
        assert!(matches!(ub_read_sync(&table, bad_addr, &mut buf), Err(UbError::AddrInvalid)));
    }
}
