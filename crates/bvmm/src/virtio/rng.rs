// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! virtio-rng (Virtio 1.2, section 5.4): the guest's hardware random number
//! source, filled from the host's.

use super::queue::Queue;
use super::{VirtioDevice, TYPE_RNG};
use crate::memory::GuestMemory;

/// The most handed over for one request, whatever the guest asks for.
const MAX_REQUEST: usize = 4096;

/// Fill `buf` from the operating system's random source.
pub fn host_random(buf: &mut [u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(buf)
    }
    #[cfg(windows)]
    {
        #[link(name = "advapi32")]
        extern "system" {
            // RtlGenRandom.
            fn SystemFunction036(buffer: *mut u8, length: u32) -> u8;
        }
        // SAFETY: the pointer and length describe `buf`.
        let ok = unsafe { SystemFunction036(buf.as_mut_ptr(), buf.len() as u32) };
        if ok != 0 {
            Ok(())
        } else {
            Err(std::io::Error::other("the system random source failed"))
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = buf;
        Err(std::io::Error::other("no random source on this platform"))
    }
}

#[derive(Default)]
pub struct Rng;

impl VirtioDevice for Rng {
    fn device_type(&self) -> u32 {
        TYPE_RNG
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![64]
    }

    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn notify(&mut self, _index: usize, queue: &mut Queue, mem: &GuestMemory) -> bool {
        let mut any = false;
        while let Ok(Some(chain)) = queue.pop(mem) {
            let wanted: usize = chain
                .descs
                .iter()
                .filter(|d| d.writable)
                .map(|d| d.len as usize)
                .sum();
            let mut bytes = vec![0u8; wanted.min(MAX_REQUEST)];
            // If the host cannot supply randomness, give nothing rather than
            // something predictable.
            let written = match host_random(&mut bytes) {
                Ok(()) => chain.write_response(mem, &bytes).unwrap_or(0),
                Err(_) => 0,
            };
            if queue.push_used(mem, chain.head, written).is_err() {
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
    fn fills_the_guests_buffer_with_host_randomness() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut rng = Rng;
        let mut q = queue(8);
        put_desc(&mem, 0, 0x8000, 64, 2, 0);
        publish(&mem, 8, 0, 0);
        assert!(rng.notify(0, &mut q, &mem));
        assert_eq!(mem.read_u32(USED + 8).unwrap(), 64);
        let mut a = [0u8; 64];
        mem.read(0x8000, &mut a).unwrap();
        publish(&mem, 8, 1, 0);
        rng.notify(0, &mut q, &mem);
        let mut b = [0u8; 64];
        mem.read(0x8000, &mut b).unwrap();
        assert_ne!(a, [0u8; 64], "64 zero bytes by chance: 2^-512");
        assert_ne!(a, b, "two draws differ");
    }

    #[test]
    fn a_huge_request_is_capped() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut rng = Rng;
        let mut q = queue(8);
        put_desc(&mem, 0, 0x10000, 1 << 20, 2, 0);
        publish(&mem, 8, 0, 0);
        rng.notify(0, &mut q, &mem);
        assert_eq!(mem.read_u32(USED + 8).unwrap(), MAX_REQUEST as u32);
    }
}
