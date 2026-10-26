// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Guest RAM: one host allocation mapped at guest-physical address 0.

use std::alloc::{alloc_zeroed, dealloc, Layout};

use crate::layout::MMIO_HOLE_START;
use crate::{Error, Result};

pub struct GuestMemory {
    ptr: *mut u8,
    layout: Layout,
}

// The allocation is plain memory owned by this struct; vCPU threads and the
// hypervisor access it concurrently, exactly like real RAM.
unsafe impl Send for GuestMemory {}
unsafe impl Sync for GuestMemory {}

impl GuestMemory {
    /// Allocate `size` bytes of zeroed guest RAM. RAM must end below the
    /// 32-bit MMIO hole, which keeps the address map a single region.
    pub fn new(size: u64) -> Result<GuestMemory> {
        if size < 64 << 20 {
            return Err(Error::new("guest memory must be at least 64 MiB"));
        }
        if size > MMIO_HOLE_START {
            return Err(Error::new(format!(
                "the native hypervisor supports up to {} MiB of guest memory for now",
                MMIO_HOLE_START >> 20
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

    pub fn size(&self) -> u64 {
        self.layout.size() as u64
    }

    pub fn host_ptr(&self) -> *mut u8 {
        self.ptr
    }

    fn check(&self, gpa: u64, len: usize) -> Result<usize> {
        let end = gpa.checked_add(len as u64).filter(|&e| e <= self.size());
        match end {
            Some(_) => Ok(gpa as usize),
            None => Err(Error::new(format!(
                "guest access {gpa:#x}+{len:#x} is outside RAM"
            ))),
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
        assert!(GuestMemory::new(4 << 30).is_err());
    }
}
