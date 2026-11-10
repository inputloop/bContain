// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! virtio-vsock (Virtio 1.2, section 5.10) with no host services yet.
//!
//! The guest gets a working socket family and an address, so services that
//! listen on vsock can start. Any connection the guest tries to open to the
//! host is answered with a reset, the way a closed port answers. Carrying
//! real streams (adb, for one) comes later.

use std::collections::VecDeque;

use super::queue::Queue;
use super::{VirtioDevice, TYPE_VSOCK};
use crate::memory::GuestMemory;

const HDR: usize = 44;
const OP_RST: u16 = 3;
/// Replies waiting for the guest to offer a buffer are dropped beyond this.
const MAX_PENDING: usize = 256;

pub struct Vsock {
    guest_cid: u64,
    replies: VecDeque<[u8; HDR]>,
}

impl Vsock {
    /// A device whose guest has address `guest_cid` (3 or above).
    pub fn new(guest_cid: u64) -> Vsock {
        Vsock {
            guest_cid: guest_cid.max(3),
            replies: VecDeque::new(),
        }
    }

    /// The reset that answers packet `p`, or None if `p` is itself a reset.
    fn reset_for(p: &[u8]) -> Option<[u8; HDR]> {
        if p.len() < HDR || u16::from_le_bytes([p[30], p[31]]) == OP_RST {
            return None;
        }
        let mut r = [0u8; HDR];
        r[0..8].copy_from_slice(&p[8..16]); // src_cid  <- their dst_cid
        r[8..16].copy_from_slice(&p[0..8]); // dst_cid  <- their src_cid
        r[16..20].copy_from_slice(&p[20..24]); // src_port <- their dst_port
        r[20..24].copy_from_slice(&p[16..20]); // dst_port <- their src_port
        r[28..30].copy_from_slice(&p[28..30]); // same socket type
        r[30..32].copy_from_slice(&OP_RST.to_le_bytes());
        Some(r)
    }
}

impl VirtioDevice for Vsock {
    fn device_type(&self) -> u32 {
        TYPE_VSOCK
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![128, 128, 16] // to the guest, from the guest, events
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let cfg = self.guest_cid.to_le_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn notify(&mut self, index: usize, queue: &mut Queue, mem: &GuestMemory) -> bool {
        if index != 1 {
            return false; // receive buffers and event buffers: see `poll`
        }
        let mut any = false;
        while let Ok(Some(chain)) = queue.pop(mem) {
            let packet = chain.read_request(mem, HDR).unwrap_or_default();
            if let Some(reset) = Vsock::reset_for(&packet) {
                if self.replies.len() < MAX_PENDING {
                    self.replies.push_back(reset);
                }
            }
            if queue.push_used(mem, chain.head, 0).is_err() {
                break;
            }
            any = true;
        }
        any
    }

    fn poll(&mut self, queues: &mut [Queue], mem: &GuestMemory) -> Vec<usize> {
        let Some(rx) = queues.get_mut(0) else {
            return Vec::new();
        };
        let mut delivered = false;
        while let Some(reply) = self.replies.front() {
            let Ok(Some(chain)) = rx.pop(mem) else {
                break;
            };
            let written = chain.write_response(mem, reply).unwrap_or(0);
            if rx.push_used(mem, chain.head, written).is_err() {
                break;
            }
            self.replies.pop_front();
            delivered = true;
        }
        if delivered {
            vec![0]
        } else {
            Vec::new()
        }
    }

    fn reset(&mut self) {
        self.replies.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(src_cid: u64, dst_cid: u64, src_port: u32, dst_port: u32, op: u16) -> [u8; HDR] {
        let mut p = [0u8; HDR];
        p[0..8].copy_from_slice(&src_cid.to_le_bytes());
        p[8..16].copy_from_slice(&dst_cid.to_le_bytes());
        p[16..20].copy_from_slice(&src_port.to_le_bytes());
        p[20..24].copy_from_slice(&dst_port.to_le_bytes());
        p[28..30].copy_from_slice(&1u16.to_le_bytes());
        p[30..32].copy_from_slice(&op.to_le_bytes());
        p
    }

    /// A ready queue of 8 with its rings at `base`, and a way to offer it
    /// descriptor 0 pointing at `addr`.
    fn queue_at(base: u64) -> Queue {
        let mut q = Queue::new(64);
        q.size = 8;
        q.desc = base;
        q.avail = base + 0x1000;
        q.used = base + 0x2000;
        q.ready = true;
        q
    }

    fn offer(mem: &GuestMemory, base: u64, addr: u64, len: u32, writable: bool) {
        mem.write_u64(base, addr).unwrap();
        mem.write_u32(base + 8, len).unwrap();
        mem.write_u16(base + 12, if writable { 2 } else { 0 })
            .unwrap();
        mem.write_u16(base + 0x1000 + 4, 0).unwrap(); // ring[0] = descriptor 0
        mem.write_u16(base + 0x1000 + 2, 1).unwrap(); // one request available
    }

    #[test]
    fn a_connection_to_the_host_is_reset() {
        const RX: u64 = 0x10_0000;
        const TX: u64 = 0x20_0000;
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut v = Vsock::new(3);
        let mut cfg = [0u8; 8];
        v.read_config(0, &mut cfg);
        assert_eq!(u64::from_le_bytes(cfg), 3);

        let mut queues = vec![queue_at(RX), queue_at(TX), queue_at(0x30_0000)];
        // The guest (cid 3, port 1234) asks to connect to the host (cid 2) port 6900.
        mem.write(0x8000, &packet(3, 2, 1234, 6900, 1)).unwrap();
        offer(&mem, TX, 0x8000, HDR as u32, false);
        assert!(v.notify(1, &mut queues[1], &mem));
        assert_eq!(mem.read_u16(TX + 0x2000 + 2).unwrap(), 1, "request taken");
        assert!(
            v.poll(&mut queues, &mem).is_empty(),
            "no receive buffer yet"
        );

        offer(&mem, RX, 0x9000, 256, true);
        assert_eq!(v.poll(&mut queues, &mem), vec![0]);
        assert_eq!(mem.read_u32(RX + 0x2000 + 8).unwrap(), HDR as u32);
        let mut got = [0u8; HDR];
        mem.read(0x9000, &mut got).unwrap();
        assert_eq!(got, packet(2, 3, 6900, 1234, OP_RST));
        assert!(
            v.poll(&mut queues, &mem).is_empty(),
            "one reset per request"
        );
    }

    #[test]
    fn resets_are_not_answered_and_short_packets_are_ignored() {
        assert!(Vsock::reset_for(&packet(3, 2, 1, 2, OP_RST)).is_none());
        assert!(Vsock::reset_for(&[0u8; 10]).is_none());
        assert!(
            Vsock::reset_for(&packet(3, 2, 1, 2, 5)).is_some(),
            "data to nowhere"
        );
    }
}
