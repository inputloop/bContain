// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! virtio-console (Virtio 1.2, section 5.3) with nobody at the other end:
//! what the guest writes is discarded and it never receives anything.
//!
//! Android's virtual-device services each expect a console port for a host
//! helper (Bluetooth, GNSS, sensors and so on). With a port that exists but
//! stays silent they start and wait, instead of crashing for want of a
//! device node.

use super::queue::Queue;
use super::{VirtioDevice, TYPE_CONSOLE};
use crate::memory::GuestMemory;

#[derive(Default)]
pub struct SinkConsole;

impl VirtioDevice for SinkConsole {
    fn device_type(&self) -> u32 {
        TYPE_CONSOLE
    }

    fn features(&self) -> u64 {
        0 // one port, no size, no multiport control
    }

    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![64, 64] // to the guest (never used), from the guest
    }

    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn notify(&mut self, index: usize, queue: &mut Queue, mem: &GuestMemory) -> bool {
        if index != 1 {
            return false; // buffers offered for input: there will never be any
        }
        let mut any = false;
        while let Ok(Some(chain)) = queue.pop(mem) {
            if queue.push_used(mem, chain.head, 0).is_err() {
                break;
            }
            any = true;
        }
        any
    }
}

#[cfg(test)]
mod tests {
    use super::super::queue::tests::{publish, put_desc, queue, USED};
    use super::*;

    #[test]
    fn output_is_swallowed_and_input_never_comes() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut c = SinkConsole;
        let mut q = queue(8);
        put_desc(&mem, 0, 0x8000, 100, 0, 0);
        publish(&mem, 8, 0, 0);
        assert!(!c.notify(0, &mut q, &mem), "the input queue is left alone");
        assert_eq!(mem.read_u16(USED + 2).unwrap(), 0);
        assert!(c.notify(1, &mut q, &mem));
        assert_eq!(mem.read_u16(USED + 2).unwrap(), 1);
        assert_eq!(mem.read_u32(USED + 8).unwrap(), 0, "nothing written back");
    }
}
