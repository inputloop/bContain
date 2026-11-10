// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! A split virtqueue (Virtio 1.2, section 2.7): the descriptor table, the
//! available ring the driver fills, and the used ring the device fills.
//! Everything read from the guest is treated as hostile: indexes are bounds
//! checked and descriptor chains cannot loop.

use std::sync::atomic::{fence, Ordering};

use crate::memory::GuestMemory;
use crate::Result;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
const DESC_F_INDIRECT: u16 = 4;
const DESC_SIZE: u64 = 16;

/// One buffer of a request: guest address, length, and whether the device
/// may write to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    pub writable: bool,
}

/// A request: the head descriptor's index and its buffers in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    pub head: u16,
    pub descs: Vec<Desc>,
}

#[derive(Debug, Clone)]
pub struct Queue {
    pub max_size: u16,
    pub size: u16,
    pub ready: bool,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    /// MSI-X vector the driver assigned, or 0xffff for none.
    pub msix_vector: u16,
    next_avail: u16,
    next_used: u16,
}

impl Queue {
    pub fn new(max_size: u16) -> Queue {
        Queue {
            max_size,
            size: max_size,
            ready: false,
            desc: 0,
            avail: 0,
            used: 0,
            msix_vector: 0xffff,
            next_avail: 0,
            next_used: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Queue::new(self.max_size);
    }

    /// The queue size is usable: a power of two no larger than the maximum.
    fn valid(&self) -> bool {
        self.ready && self.size != 0 && self.size <= self.max_size && self.size.is_power_of_two()
    }

    /// Take the next request the driver made available, if any.
    pub fn pop(&mut self, mem: &GuestMemory) -> Result<Option<Chain>> {
        if !self.valid() {
            return Ok(None);
        }
        fence(Ordering::SeqCst);
        let avail_idx = mem.read_u16(self.avail + 2)?;
        if avail_idx == self.next_avail {
            return Ok(None);
        }
        let slot = (self.next_avail % self.size) as u64;
        let head = mem.read_u16(self.avail + 4 + slot * 2)?;
        self.next_avail = self.next_avail.wrapping_add(1);

        let mut descs = Vec::new();
        let mut index = head;
        // A chain can never be longer than the queue; this also stops loops.
        for _ in 0..self.size {
            if index >= self.size {
                break;
            }
            let at = self.desc + index as u64 * DESC_SIZE;
            let addr = mem.read_u64(at)?;
            let len = mem.read_u32(at + 8)?;
            let flags = mem.read_u16(at + 12)?;
            let next = mem.read_u16(at + 14)?;
            if flags & DESC_F_INDIRECT != 0 {
                // Not offered (VIRTIO_F_INDIRECT_DESC is never negotiated).
                break;
            }
            descs.push(Desc {
                addr,
                len,
                writable: flags & DESC_F_WRITE != 0,
            });
            if flags & DESC_F_NEXT == 0 {
                return Ok(Some(Chain { head, descs }));
            }
            index = next;
        }
        // Malformed chain: hand it back empty so the driver is not left waiting.
        Ok(Some(Chain {
            head,
            descs: Vec::new(),
        }))
    }

    /// Return a request to the driver, saying `len` bytes were written into
    /// its writable buffers.
    pub fn push_used(&mut self, mem: &GuestMemory, head: u16, len: u32) -> Result<()> {
        if !self.valid() {
            return Ok(());
        }
        let slot = (self.next_used % self.size) as u64;
        let at = self.used + 4 + slot * 8;
        mem.write_u32(at, head as u32)?;
        mem.write_u32(at + 4, len)?;
        self.next_used = self.next_used.wrapping_add(1);
        // The element must be visible before the index that publishes it.
        fence(Ordering::SeqCst);
        mem.write_u16(self.used + 2, self.next_used)?;
        fence(Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const DESC: u64 = 0x1000;
    pub const AVAIL: u64 = 0x2000;
    pub const USED: u64 = 0x3000;

    pub fn queue(size: u16) -> Queue {
        let mut q = Queue::new(256);
        q.size = size;
        q.desc = DESC;
        q.avail = AVAIL;
        q.used = USED;
        q.ready = true;
        q
    }

    pub fn put_desc(mem: &GuestMemory, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let at = DESC + i as u64 * 16;
        mem.write_u64(at, addr).unwrap();
        mem.write_u32(at + 8, len).unwrap();
        mem.write_u16(at + 12, flags).unwrap();
        mem.write_u16(at + 14, next).unwrap();
    }

    /// Make descriptor `head` available as the `n`th request.
    pub fn publish(mem: &GuestMemory, size: u16, n: u16, head: u16) {
        mem.write_u16(AVAIL + 4 + (n % size) as u64 * 2, head)
            .unwrap();
        mem.write_u16(AVAIL + 2, n.wrapping_add(1)).unwrap();
    }

    #[test]
    fn pops_a_chain_and_returns_it() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut q = queue(8);
        assert_eq!(q.pop(&mem).unwrap(), None);
        put_desc(&mem, 3, 0x8000, 16, DESC_F_NEXT, 5);
        put_desc(&mem, 5, 0x9000, 512, DESC_F_NEXT | DESC_F_WRITE, 1);
        put_desc(&mem, 1, 0xa000, 1, DESC_F_WRITE, 0);
        publish(&mem, 8, 0, 3);
        let c = q.pop(&mem).unwrap().unwrap();
        assert_eq!(c.head, 3);
        assert_eq!(c.descs.len(), 3);
        assert_eq!(
            c.descs[1],
            Desc {
                addr: 0x9000,
                len: 512,
                writable: true
            }
        );
        assert_eq!(q.pop(&mem).unwrap(), None, "nothing else is available");
        q.push_used(&mem, 3, 513).unwrap();
        assert_eq!(mem.read_u16(USED + 2).unwrap(), 1);
        assert_eq!(mem.read_u32(USED + 4).unwrap(), 3);
        assert_eq!(mem.read_u32(USED + 8).unwrap(), 513);
    }

    #[test]
    fn hostile_chains_do_not_loop_or_escape() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut q = queue(4);
        // A loop: 0 -> 1 -> 0.
        put_desc(&mem, 0, 0x8000, 1, DESC_F_NEXT, 1);
        put_desc(&mem, 1, 0x8000, 1, DESC_F_NEXT, 0);
        publish(&mem, 4, 0, 0);
        assert!(q.pop(&mem).unwrap().unwrap().descs.is_empty());
        // A head beyond the table.
        publish(&mem, 4, 1, 200);
        assert!(q.pop(&mem).unwrap().unwrap().descs.is_empty());
        // An indirect descriptor, which is never negotiated.
        put_desc(&mem, 2, 0x8000, 16, DESC_F_INDIRECT, 0);
        publish(&mem, 4, 2, 2);
        assert!(q.pop(&mem).unwrap().unwrap().descs.is_empty());
    }

    #[test]
    fn a_queue_that_is_not_ready_or_badly_sized_yields_nothing() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut q = queue(8);
        put_desc(&mem, 0, 0x8000, 1, 0, 0);
        publish(&mem, 8, 0, 0);
        q.ready = false;
        assert_eq!(q.pop(&mem).unwrap(), None);
        q.ready = true;
        q.size = 6; // not a power of two
        assert_eq!(q.pop(&mem).unwrap(), None);
        q.size = 512; // larger than the maximum
        assert_eq!(q.pop(&mem).unwrap(), None);
    }

    #[test]
    fn ring_indexes_wrap() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut q = queue(2);
        put_desc(&mem, 0, 0x8000, 1, 0, 0);
        for n in 0..5u16 {
            publish(&mem, 2, n, 0);
            assert_eq!(q.pop(&mem).unwrap().unwrap().head, 0);
            q.push_used(&mem, 0, 0).unwrap();
        }
        assert_eq!(mem.read_u16(USED + 2).unwrap(), 5);
    }
}
