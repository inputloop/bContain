// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! virtio-console (Virtio 1.2, section 5.3): one serial port per device,
//! `/dev/hvcN` in a Linux guest.
//!
//! A port either has nobody at the other end ([`Console::sink`]: output is
//! discarded and nothing ever arrives), or a handler that is given what the
//! guest writes and returns what to send back ([`Console::responder`]).
//!
//! Android's virtual-device services each expect a port for a host helper.
//! With a silent port they start and wait instead of crashing for want of a
//! device node; with a responder the helper's side of the conversation can
//! be supplied by the embedder.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::queue::Queue;
use super::{VirtioDevice, TYPE_CONSOLE};
use crate::memory::GuestMemory;

/// Given bytes the guest wrote, returns bytes to send back (possibly none).
pub type Handler = Box<dyn FnMut(&[u8]) -> Vec<u8> + Send>;

/// Most taken from the guest in one write, and most held for it to read.
const MAX_WRITE: usize = 64 << 10;
const MAX_PENDING: usize = 1 << 20;

#[derive(Clone, Default)]
pub struct Console {
    handler: Option<Arc<Mutex<Handler>>>,
    /// Bytes waiting for the guest to offer a buffer.
    to_guest: Arc<Mutex<VecDeque<u8>>>,
}

impl Console {
    /// A port with nobody at the other end.
    pub fn sink() -> Console {
        Console::default()
    }

    /// A port whose other end is `handler`.
    pub fn responder(handler: impl FnMut(&[u8]) -> Vec<u8> + Send + 'static) -> Console {
        Console {
            handler: Some(Arc::new(Mutex::new(Box::new(handler)))),
            to_guest: Arc::default(),
        }
    }
}

impl VirtioDevice for Console {
    fn device_type(&self) -> u32 {
        TYPE_CONSOLE
    }

    fn features(&self) -> u64 {
        0 // one port, no size, no multiport control
    }

    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![64, 64] // to the guest, from the guest
    }

    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn notify(&mut self, index: usize, queue: &mut Queue, mem: &GuestMemory) -> bool {
        if index != 1 {
            return false; // buffers offered for input: `poll` fills them
        }
        let mut any = false;
        while let Ok(Some(chain)) = queue.pop(mem) {
            if let Some(handler) = &self.handler {
                let written = chain.read_request(mem, MAX_WRITE).unwrap_or_default();
                let reply = (handler.lock().unwrap())(&written);
                let mut pending = self.to_guest.lock().unwrap();
                let room = MAX_PENDING.saturating_sub(pending.len());
                pending.extend(reply.into_iter().take(room));
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
        let mut pending = self.to_guest.lock().unwrap();
        let mut delivered = false;
        while !pending.is_empty() {
            let Ok(Some(chain)) = rx.pop(mem) else {
                break;
            };
            let room: usize = chain
                .descs
                .iter()
                .filter(|d| d.writable)
                .map(|d| d.len as usize)
                .sum();
            let take = room.min(pending.len());
            let bytes: Vec<u8> = pending.iter().take(take).copied().collect();
            let written = chain.write_response(mem, &bytes).unwrap_or(0);
            pending.drain(..written as usize);
            if rx.push_used(mem, chain.head, written).is_err() {
                break;
            }
            delivered = true;
        }
        if delivered {
            vec![0]
        } else {
            Vec::new()
        }
    }

    fn reset(&mut self) {
        self.to_guest.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue_at(base: u64) -> Queue {
        let mut q = Queue::new(64);
        q.size = 8;
        q.desc = base;
        q.avail = base + 0x1000;
        q.used = base + 0x2000;
        q.ready = true;
        q
    }

    /// Offer descriptor 0 at `addr` as the queue's `n`th request.
    fn offer(mem: &GuestMemory, base: u64, n: u16, addr: u64, len: u32, writable: bool) {
        mem.write_u64(base, addr).unwrap();
        mem.write_u32(base + 8, len).unwrap();
        mem.write_u16(base + 12, if writable { 2 } else { 0 })
            .unwrap();
        mem.write_u16(base + 0x1000 + 4 + (n % 8) as u64 * 2, 0)
            .unwrap();
        mem.write_u16(base + 0x1000 + 2, n + 1).unwrap();
    }

    const RX: u64 = 0x10_0000;
    const TX: u64 = 0x20_0000;

    #[test]
    fn a_sink_swallows_output_and_never_has_input() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut c = Console::sink();
        let mut queues = vec![queue_at(RX), queue_at(TX)];
        mem.write(0x8000, b"hello").unwrap();
        offer(&mem, TX, 0, 0x8000, 5, false);
        offer(&mem, RX, 0, 0x9000, 64, true);
        assert!(!c.notify(0, &mut queues[0], &mem), "input kicks do nothing");
        assert!(c.notify(1, &mut queues[1], &mem));
        assert_eq!(mem.read_u16(TX + 0x2000 + 2).unwrap(), 1);
        assert!(c.poll(&mut queues, &mem).is_empty());
        assert_eq!(
            mem.read_u16(RX + 0x2000 + 2).unwrap(),
            0,
            "buffer left with the guest"
        );
    }

    #[test]
    fn a_responder_sees_output_and_its_reply_reaches_the_guest() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut c = Console::responder(|bytes| {
            let mut reply = b"got:".to_vec();
            reply.extend_from_slice(bytes);
            reply
        });
        let mut queues = vec![queue_at(RX), queue_at(TX)];
        mem.write(0x8000, b"ping").unwrap();
        offer(&mem, TX, 0, 0x8000, 4, false);
        assert!(c.notify(1, &mut queues[1], &mem));
        assert!(c.poll(&mut queues, &mem).is_empty(), "no input buffer yet");

        // A buffer smaller than the reply: it arrives in two reads.
        offer(&mem, RX, 0, 0x9000, 5, true);
        assert_eq!(c.poll(&mut queues, &mem), vec![0]);
        assert_eq!(mem.read_u32(RX + 0x2000 + 8).unwrap(), 5);
        let mut got = [0u8; 5];
        mem.read(0x9000, &mut got).unwrap();
        assert_eq!(&got, b"got:p");
        offer(&mem, RX, 1, 0x9000, 64, true);
        assert_eq!(c.poll(&mut queues, &mem), vec![0]);
        assert_eq!(mem.read_u32(RX + 0x2000 + 8 + 8).unwrap(), 3);
        mem.read(0x9000, &mut got[..3]).unwrap();
        assert_eq!(&got[..3], b"ing");
        assert!(c.poll(&mut queues, &mem).is_empty());
    }

    #[test]
    fn replies_the_guest_never_reads_are_bounded() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut c = Console::responder(|_| vec![0u8; 400 << 10]);
        let mut q = queue_at(TX);
        for n in 0..4 {
            offer(&mem, TX, n, 0x8000, 1, false);
            c.notify(1, &mut q, &mem);
        }
        assert_eq!(c.to_guest.lock().unwrap().len(), MAX_PENDING);
    }
}
