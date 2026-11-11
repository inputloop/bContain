// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Guest RAM: one host allocation. The guest sees its first part at
//! guest-physical address 0, up to the 32-bit device hole, and the rest from
//! 4 GiB upwards, as on a PC.

use std::alloc::{alloc_zeroed, dealloc, Layout};

use crate::layout::{HIGH_MEMORY_START, MMIO_HOLE_START};
use crate::{Error, Result};

/// A generous ceiling; nothing here needs more, and it bounds a bad argument.
pub const MAX_SIZE: u64 = 256 << 30;

pub struct GuestMemory {
    ptr: *mut u8,
    layout: Layout,
}

// The allocation is plain memory owned by this struct; vCPU threads and the
// hypervisor access it concurrently, exactly like real RAM.
unsafe impl Send for GuestMemory {}
unsafe impl Sync for GuestMemory {}

impl GuestMemory {
    /// Allocate `size` bytes of zeroed guest RAM, a whole number of pages.
    pub fn new(size: u64) -> Result<GuestMemory> {
        if size < 64 << 20 {
            return Err(Error::new("guest memory must be at least 64 MiB"));
        }
        if size > MAX_SIZE || !size.is_multiple_of(4096) {
            return Err(Error::new(format!(
                "guest memory must be a whole number of pages, at most {} GiB",
                MAX_SIZE >> 30
            )));
        }
        let layout = Layout::from_size_align(size as usize, 4096)
            .map_err(|e| Error::new(format!("bad memory size: {e}")))?;
        // SAFETY: the layout has non-zero size.
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(Error::new(format!(
                "could not allocate {} MiB of guest memory",
                size >> 20
            )));
        }
        Ok(GuestMemory { ptr, layout })
    }

    /// All guest RAM, in bytes.
    pub fn size(&self) -> u64 {
        self.layout.size() as u64
    }

    /// RAM below the 32-bit device hole, which starts at address 0.
    pub fn low_size(&self) -> u64 {
        self.size().min(MMIO_HOLE_START)
    }

    /// RAM placed from 4 GiB upwards.
    pub fn high_size(&self) -> u64 {
        self.size() - self.low_size()
    }

    /// The pieces to map into the guest: (guest address, host pointer, bytes).
    pub fn regions(&self) -> Vec<(u64, *mut u8, u64)> {
        let mut r = vec![(0, self.ptr, self.low_size())];
        if self.high_size() > 0 {
            // SAFETY: low_size() is within the allocation.
            let high = unsafe { self.ptr.add(self.low_size() as usize) };
            r.push((HIGH_MEMORY_START, high, self.high_size()));
        }
        r
    }

    /// Is `gpa..gpa+len` entirely guest RAM?
    pub fn contains(&self, gpa: u64, len: usize) -> bool {
        self.check(gpa, len).is_ok()
    }

    /// Offset into the allocation of a guest range that lies wholly within
    /// one of the two RAM pieces.
    fn check(&self, gpa: u64, len: usize) -> Result<usize> {
        let outside = || Error::new(format!("guest access {gpa:#x}+{len:#x} is outside RAM"));
        let end = gpa.checked_add(len as u64).ok_or_else(outside)?;
        if end <= self.low_size() {
            Ok(gpa as usize)
        } else if gpa >= HIGH_MEMORY_START && end - HIGH_MEMORY_START <= self.high_size() {
            Ok((self.low_size() + (gpa - HIGH_MEMORY_START)) as usize)
        } else {
            Err(outside())
        }
    }

    pub fn write(&self, gpa: u64, data: &[u8]) -> Result<()> {
        let off = self.check(gpa, data.len())?;
        // SAFETY: bounds checked above; the source cannot overlap guest RAM.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(off), data.len()) };
        Ok(())
    }

    pub fn read(&self, gpa: u64, out: &mut [u8]) -> Result<()> {
        let off = self.check(gpa, out.len())?;
        // SAFETY: bounds checked above.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr.add(off), out.as_mut_ptr(), out.len()) };
        Ok(())
    }

    pub fn read_u16(&self, gpa: u64) -> Result<u16> {
        let mut b = [0u8; 2];
        self.read(gpa, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    pub fn write_u16(&self, gpa: u64, v: u16) -> Result<()> {
        self.write(gpa, &v.to_le_bytes())
    }

    pub fn read_u32(&self, gpa: u64) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read(gpa, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    pub fn write_u32(&self, gpa: u64, v: u32) -> Result<()> {
        self.write(gpa, &v.to_le_bytes())
    }

    pub fn write_u64(&self, gpa: u64, v: u64) -> Result<()> {
        self.write(gpa, &v.to_le_bytes())
    }

    pub fn read_u64(&self, gpa: u64) -> Result<u64> {
        let mut b = [0u8; 8];
        self.read(gpa, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

/// Maps host memory into the running guest's address space, and takes it
/// out again. Each hypervisor backend supplies one; devices that share host
/// memory with the guest (the GPU's host-visible region) use it.
pub trait Mapper: Send + Sync {
    /// Make `len` bytes at `host` appear at guest address `gpa`. All three
    /// must be multiples of the page size.
    ///
    /// # Safety
    /// The host range must stay valid until it is unmapped.
    unsafe fn map(&self, gpa: u64, host: *mut u8, len: u64) -> crate::Result<()>;
    /// Remove a mapping made at `gpa` with `map`.
    fn unmap(&self, gpa: u64, len: u64) -> crate::Result<()>;
}

/// Where a device finds the machine's [`Mapper`]: empty until the
/// hypervisor backend has started.
pub type MapperSlot = std::sync::Arc<std::sync::OnceLock<Box<dyn Mapper>>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds() {
        let m = GuestMemory::new(64 << 20).unwrap();
        m.write_u64(0x1000, 0xdead_beef).unwrap();
        assert_eq!(m.read_u64(0x1000).unwrap(), 0xdead_beef);
        assert!(m.write(m.size() - 4, &[0; 8]).is_err());
        assert!(m.write(u64::MAX - 2, &[0; 8]).is_err());
        assert!(
            GuestMemory::new((64 << 20) + 1).is_err(),
            "whole pages only"
        );
        assert!(GuestMemory::new(MAX_SIZE + 4096).is_err());
    }

    #[test]
    fn memory_above_the_device_hole_starts_at_4_gib() {
        // 3 GiB + 64 MiB: the last 64 MiB live at 4 GiB.
        let m = GuestMemory::new(MMIO_HOLE_START + (64 << 20)).unwrap();
        assert_eq!(m.low_size(), MMIO_HOLE_START);
        assert_eq!(m.high_size(), 64 << 20);
        let r = m.regions();
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].0, r[0].2), (0, MMIO_HOLE_START));
        assert_eq!((r[1].0, r[1].2), (HIGH_MEMORY_START, 64 << 20));
        // The hole itself is not RAM.
        assert!(!m.contains(MMIO_HOLE_START, 1));
        assert!(
            !m.contains(MMIO_HOLE_START - 4, 8),
            "no access may straddle the hole"
        );
        assert!(m.contains(MMIO_HOLE_START - 8, 8));
        // Low and high memory are different bytes.
        m.write_u64(MMIO_HOLE_START - 8, 1).unwrap();
        m.write_u64(HIGH_MEMORY_START, 2).unwrap();
        assert_eq!(m.read_u64(MMIO_HOLE_START - 8).unwrap(), 1);
        assert_eq!(m.read_u64(HIGH_MEMORY_START).unwrap(), 2);
        let last = HIGH_MEMORY_START + (64 << 20) - 8;
        m.write_u64(last, 3).unwrap();
        assert_eq!(m.read_u64(last).unwrap(), 3);
        assert!(m.write_u64(last + 8, 4).is_err());
        assert!(GuestMemory::new(64 << 20).unwrap().regions().len() == 1);
    }
}
