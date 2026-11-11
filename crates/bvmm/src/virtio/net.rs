// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! virtio-net (Virtio 1.2, section 5.1): an Ethernet card.
//!
//! The device only carries frames. Whoever made it is given every frame the
//! guest sends, and holds a [`NetSender`] to hand frames to the guest; what
//! network lies behind is their business (see [`crate::net`]).
//!
//! No offloads are offered, so every frame in both directions is complete
//! and checksummed, at the cost of speed.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use super::queue::Queue;
use super::{VirtioDevice, TYPE_NET};
use crate::memory::GuestMemory;

/// `VIRTIO_NET_F_MAC`: the device has a MAC address in its configuration.
const F_MAC: u64 = 1 << 5;
/// `struct virtio_net_hdr` in the 1.x layout, before every frame.
const HEADER: usize = 12;
/// The largest Ethernet frame carried (1500 bytes of payload).
pub const MAX_FRAME: usize = 1514;
/// Frames held for the guest while it offers no buffers; more are dropped,
/// as a real network would.
const MAX_PENDING: usize = 1024;

/// What the guest's frames are given to.
type Sink = Box<dyn Fn(Vec<u8>) + Send + Sync>;

struct Shared {
    to_guest: Mutex<VecDeque<Vec<u8>>>,
    from_guest: Sink,
    wake: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

#[derive(Clone)]
pub struct Net {
    mac: [u8; 6],
    shared: Arc<Shared>,
}

/// Hands frames to the guest. Usable from any thread, at any time.
#[derive(Clone)]
pub struct NetSender {
    shared: Weak<Shared>,
}

impl NetSender {
    /// Whether the card still exists.
    pub fn is_alive(&self) -> bool {
        self.shared.strong_count() > 0
    }

    /// Queue one Ethernet frame for the guest. False once the card is gone.
    pub fn send(&self, frame: Vec<u8>) -> bool {
        let Some(shared) = self.shared.upgrade() else {
            return false;
        };
        {
            let mut pending = shared.to_guest.lock().unwrap();
            if pending.len() < MAX_PENDING && frame.len() <= MAX_FRAME {
                pending.push_back(frame);
            }
        }
        if let Some(wake) = shared.wake.get() {
            wake();
        }
        true
    }
}

impl Net {
    /// A card with address `mac`. `from_guest` is called with each frame
    /// the guest sends, on the thread that runs the guest: it must not block.
    pub fn new(
        mac: [u8; 6],
        from_guest: impl Fn(Vec<u8>) + Send + Sync + 'static,
    ) -> (Net, NetSender) {
        let shared = Arc::new(Shared {
            to_guest: Mutex::default(),
            from_guest: Box::new(from_guest),
            wake: OnceLock::new(),
        });
        let sender = NetSender {
            shared: Arc::downgrade(&shared),
        };
        (Net { mac, shared }, sender)
    }

    /// Have the device ask `wake` to deliver frames whenever some arrive.
    pub(crate) fn set_wake(&self, wake: Box<dyn Fn() + Send + Sync>) {
        let _ = self.shared.wake.set(wake);
    }
}

impl VirtioDevice for Net {
    fn device_type(&self) -> u32 {
        TYPE_NET
    }

    fn features(&self) -> u64 {
        F_MAC
    }

    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![256, 256] // to the guest, from the guest
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // mac[6], status (link up), max_virtqueue_pairs, mtu.
        let mut config = [0u8; 12];
        config[..6].copy_from_slice(&self.mac);
        config[6..8].copy_from_slice(&1u16.to_le_bytes());
        config[8..10].copy_from_slice(&1u16.to_le_bytes());
        config[10..12].copy_from_slice(&1500u16.to_le_bytes());
        for (i, b) in data.iter_mut().enumerate() {
            *b = config.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn notify(&mut self, index: usize, queue: &mut Queue, mem: &GuestMemory) -> bool {
        if index != 1 {
            return false; // buffers offered for receiving: `poll` fills them
        }
        let mut any = false;
        while let Ok(Some(chain)) = queue.pop(mem) {
            if let Ok(bytes) = chain.read_request(mem, HEADER + MAX_FRAME) {
                if bytes.len() > HEADER {
                    (self.shared.from_guest)(bytes[HEADER..].to_vec());
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
        let mut pending = self.shared.to_guest.lock().unwrap();
        let mut delivered = false;
        while let Some(frame) = pending.front() {
            let Ok(Some(chain)) = rx.pop(mem) else {
                break;
            };
            let mut packet = vec![0u8; HEADER];
            packet[10] = 1; // num_buffers: this frame is in one buffer chain
            packet.extend_from_slice(frame);
            // A chain too small for the frame gets what fits: the guest sized
            // its buffers for the MTU it was told, so this does not happen.
            let written = chain.write_response(mem, &packet).unwrap_or(0);
            pending.pop_front();
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
        self.shared.to_guest.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::super::queue::tests::{publish, put_desc, queue};
    use super::*;

    const WRITE: u16 = 2;

    #[test]
    fn frames_cross_in_both_directions_with_the_header() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let s = sent.clone();
        let (mut net, tx) = Net::new([2, 0, 0, 0, 0, 1], move |f| s.lock().unwrap().push(f));
        let mut config = [0u8; 12];
        net.read_config(0, &mut config);
        assert_eq!(&config[..6], &[2, 0, 0, 0, 0, 1]);
        assert_eq!(u16::from_le_bytes([config[10], config[11]]), 1500);

        // From the guest: a 12-byte header, then the frame.
        let mut q = queue(8);
        let mut out = vec![0u8; HEADER];
        out.extend_from_slice(b"FRAME-FROM-GUEST");
        mem.write(0x5000, &out).unwrap();
        put_desc(&mem, 0, 0x5000, out.len() as u32, 0, 0);
        publish(&mem, 8, 0, 0);
        assert!(net.notify(1, &mut q, &mem));
        assert_eq!(*sent.lock().unwrap(), vec![b"FRAME-FROM-GUEST".to_vec()]);

        // To the guest: held until a buffer is offered, then written whole.
        // (Fresh memory: the test queues all live at the same addresses.)
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut queues = vec![queue(8), queue(8)];
        assert!(tx.send(b"FRAME-TO-GUEST".to_vec()));
        assert!(net.poll(&mut queues, &mem).is_empty(), "no buffer yet");
        put_desc(&mem, 0, 0x6000, 2048, WRITE, 0);
        publish(&mem, 8, 0, 0);
        assert_eq!(net.poll(&mut queues, &mem), vec![0]);
        let mut got = vec![0u8; HEADER + 14];
        mem.read(0x6000, &mut got).unwrap();
        assert_eq!(got[10], 1, "num_buffers");
        assert_eq!(&got[HEADER..], b"FRAME-TO-GUEST");

        // Once the card is gone the sender says so.
        drop(net);
        assert!(!tx.send(vec![0; 60]));
    }
}
