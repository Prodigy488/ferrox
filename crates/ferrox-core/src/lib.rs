//! Shared types: the guest bus every CPU backend and device talks through.

use std::fmt;

/// Physical address in the guest.
pub type GuestAddr = u64;

/// Why a bus access failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusError {
    /// Nothing is mapped at the address.
    Unmapped(GuestAddr),
    /// The region exists but does not accept this kind of access (a write to ROM).
    ReadOnly(GuestAddr),
    /// Access size other than 1, 2, 4 or 8 bytes.
    BadSize(usize),
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BusError::Unmapped(a) => write!(f, "no device at physical address {a:#x}"),
            BusError::ReadOnly(a) => write!(f, "write to read-only memory at {a:#x}"),
            BusError::BadSize(s) => write!(f, "unsupported access size {s}"),
        }
    }
}

impl std::error::Error for BusError {}

/// What a CPU sees: physical memory-mapped accesses and x86 port I/O.
///
/// Values are little endian; `size` is 1, 2, 4 or 8 bytes.
pub trait Bus {
    fn read(&mut self, addr: GuestAddr, size: usize) -> Result<u64, BusError>;
    fn write(&mut self, addr: GuestAddr, size: usize, value: u64) -> Result<(), BusError>;
    /// Port input. Unclaimed ports read as all ones, like an empty ISA bus.
    fn io_in(&mut self, _port: u16, size: usize) -> u64 {
        match size {
            1 => 0xff,
            2 => 0xffff,
            _ => 0xffff_ffff,
        }
    }
    fn io_out(&mut self, _port: u16, _size: usize, _value: u64) {}
}
