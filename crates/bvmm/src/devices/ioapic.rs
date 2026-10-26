// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! An 82093AA-style I/O APIC at 0xfec00000, used where the hypervisor has no
//! in-kernel one (Windows Hypervisor Platform emulates only the local APIC).

pub const PINS: usize = 24;
const VERSION: u32 = 0x11 | ((PINS as u32 - 1) << 16);

/// An interrupt the I/O APIC decided to deliver to a local APIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupt {
    pub vector: u8,
    /// 0 fixed, 1 lowest priority, 4 NMI, 5 INIT, 7 ExtINT.
    pub delivery_mode: u8,
    pub logical: bool,
    pub level: bool,
    pub destination: u8,
}

pub struct IoApic {
    id: u32,
    select: u32,
    table: [u64; PINS],
}

const MASKED: u64 = 1 << 16;
const REMOTE_IRR: u64 = 1 << 14;
const LEVEL: u64 = 1 << 15;

impl IoApic {
    pub fn new(id: u8) -> IoApic {
        IoApic {
            id: (id as u32) << 24,
            select: 0,
            table: [MASKED; PINS],
        }
    }

    fn read_reg(&self) -> u32 {
        match self.select {
            0x00 => self.id,
            0x01 => VERSION,
            0x02 => self.id,
            r @ 0x10..=0x3f => {
                let e = self.table[((r - 0x10) / 2) as usize];
                if r % 2 == 0 {
                    e as u32
                } else {
                    (e >> 32) as u32
                }
            }
            _ => 0,
        }
    }

    fn write_reg(&mut self, v: u32) {
        match self.select {
            0x00 => self.id = v & 0x0f00_0000,
            r @ 0x10..=0x3f => {
                let e = &mut self.table[((r - 0x10) / 2) as usize];
                if r % 2 == 0 {
                    // Delivery status and remote IRR are read-only.
                    let ro = *e & (REMOTE_IRR | 1 << 12);
                    *e = (*e & !0xffff_ffff) | (v as u64 & !(REMOTE_IRR | 1 << 12)) | ro;
                } else {
                    *e = (*e & 0xffff_ffff) | (v as u64) << 32;
                }
            }
            _ => {}
        }
    }

    /// MMIO read at `offset` within the 4 KiB window.
    pub fn mmio_read(&self, offset: u64) -> u32 {
        match offset {
            0x00 => self.select,
            0x10 => self.read_reg(),
            _ => 0,
        }
    }

    pub fn mmio_write(&mut self, offset: u64, v: u32) {
        match offset {
            0x00 => self.select = v & 0xff,
            0x10 => self.write_reg(v),
            0x40 => self.eoi(v as u8), // EOI register (version 0x20+, harmless here)
            _ => {}
        }
    }

    /// A device raised `pin` (edge). Returns the interrupt to deliver, if any.
    pub fn assert(&mut self, pin: usize) -> Option<Interrupt> {
        let e = *self.table.get(pin)?;
        if e & MASKED != 0 || (e & LEVEL != 0 && e & REMOTE_IRR != 0) {
            return None;
        }
        if e & LEVEL != 0 {
            self.table[pin] |= REMOTE_IRR;
        }
        Some(Interrupt {
            vector: e as u8,
            delivery_mode: ((e >> 8) & 7) as u8,
            logical: e & (1 << 11) != 0,
            level: e & LEVEL != 0,
            destination: (e >> 56) as u8,
        })
    }

    /// The local APIC signalled end-of-interrupt for `vector`.
    pub fn eoi(&mut self, vector: u8) {
        for e in self.table.iter_mut() {
            if *e as u8 == vector {
                *e &= !REMOTE_IRR;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(io: &mut IoApic, reg: u32, v: u32) {
        io.mmio_write(0x00, reg);
        io.mmio_write(0x10, v);
    }

    #[test]
    fn version_and_routing() {
        let mut io = IoApic::new(1);
        io.mmio_write(0x00, 0x01);
        assert_eq!(io.mmio_read(0x10), 0x0017_0011);
        assert_eq!(io.assert(4), None, "pins start masked");
        // Pin 4 -> vector 0x24, fixed, physical, edge, to APIC 0.
        write(&mut io, 0x10 + 8, 0x0000_0024);
        write(&mut io, 0x11 + 8, 0x0000_0000);
        let i = io.assert(4).unwrap();
        assert_eq!(
            (i.vector, i.delivery_mode, i.logical, i.level, i.destination),
            (0x24, 0, false, false, 0)
        );
        // Edge pins fire every time.
        assert!(io.assert(4).is_some());
    }

    #[test]
    fn level_waits_for_eoi() {
        let mut io = IoApic::new(1);
        write(&mut io, 0x10, 0x0000_8031); // pin 0, level, vector 0x31
        assert!(io.assert(0).is_some());
        assert!(io.assert(0).is_none(), "remote IRR blocks until EOI");
        io.eoi(0x31);
        assert!(io.assert(0).is_some());
    }
}
