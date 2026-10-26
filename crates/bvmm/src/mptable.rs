// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Intel MultiProcessor Specification 1.4 tables, which is how the guest
//! kernel finds its CPUs and the I/O APIC without ACPI.

use crate::layout::{IOAPIC_ADDR, LAPIC_ADDR, MPTABLE_ADDR};
use crate::memory::GuestMemory;
use crate::{Error, Result};

const APIC_VERSION: u8 = 0x14;
const IOAPIC_VERSION: u8 = 0x11;
/// ISA IRQs routed through the I/O APIC (pins 0..15).
const ISA_IRQS: u8 = 16;

fn checksum(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .fold(0u8, |a, b| a.wrapping_add(*b))
        .wrapping_neg()
}

/// Build the tables for `cpus` processors and write them at [`MPTABLE_ADDR`].
pub fn write(mem: &GuestMemory, cpus: u8) -> Result<()> {
    if cpus == 0 || cpus > 254 {
        return Err(Error::new("MP table supports 1..254 CPUs"));
    }
    let ioapic_id = cpus;
    let mut entries: Vec<u8> = Vec::new();
    let mut count: u16 = 0;

    for id in 0..cpus {
        let flags = 0x01 | if id == 0 { 0x02 } else { 0 }; // enabled, bootstrap
        entries.extend_from_slice(&[0, id, APIC_VERSION, flags]);
        entries.extend_from_slice(&0x0000_0600u32.to_le_bytes()); // family 6
        entries.extend_from_slice(&0x0000_0201u32.to_le_bytes()); // FPU | APIC
        entries.extend_from_slice(&[0; 8]);
        count += 1;
    }
    // Bus 0: ISA.
    entries.extend_from_slice(&[1, 0]);
    entries.extend_from_slice(b"ISA   ");
    count += 1;
    // The I/O APIC.
    entries.extend_from_slice(&[2, ioapic_id, IOAPIC_VERSION, 0x01]);
    entries.extend_from_slice(&(IOAPIC_ADDR as u32).to_le_bytes());
    count += 1;
    // I/O interrupt assignments: ExtINT on pin 0, ISA IRQ n on pin n
    // (IRQ 0, the PIT, on pin 2 as on PCs).
    let io_int =
        |kind: u8, irq: u8, pin: u8| -> [u8; 8] { [3, kind, 0, 0, 0, irq, ioapic_id, pin] };
    entries.extend_from_slice(&io_int(3, 0, 0));
    count += 1;
    for irq in 0..ISA_IRQS {
        let pin = if irq == 0 { 2 } else { irq };
        if irq == 2 {
            continue; // the cascade line on PICs; pin 2 already carries IRQ 0
        }
        entries.extend_from_slice(&io_int(0, irq, pin));
        count += 1;
    }
    // Local interrupts: ExtINT on LINT0, NMI on LINT1, all CPUs.
    entries.extend_from_slice(&[4, 3, 0, 0, 0, 0, 0xff, 0]);
    entries.extend_from_slice(&[4, 1, 0, 0, 0, 0, 0xff, 1]);
    count += 2;

    let table_addr = MPTABLE_ADDR + 16;
    let mut hdr = Vec::with_capacity(44);
    hdr.extend_from_slice(b"PCMP");
    hdr.extend_from_slice(&((44 + entries.len()) as u16).to_le_bytes());
    hdr.push(4); // spec revision 1.4
    hdr.push(0); // checksum, filled below
    hdr.extend_from_slice(b"BCONTAIN");
    hdr.extend_from_slice(b"BVMM        ");
    hdr.extend_from_slice(&0u32.to_le_bytes()); // OEM table
    hdr.extend_from_slice(&0u16.to_le_bytes());
    hdr.extend_from_slice(&count.to_le_bytes());
    hdr.extend_from_slice(&(LAPIC_ADDR as u32).to_le_bytes());
    hdr.extend_from_slice(&0u16.to_le_bytes()); // extended table length
    hdr.push(0); // extended checksum
    hdr.push(0);
    let mut table = hdr;
    table.extend_from_slice(&entries);
    table[7] = checksum(&table);

    let mut fp = Vec::with_capacity(16);
    fp.extend_from_slice(b"_MP_");
    fp.extend_from_slice(&(table_addr as u32).to_le_bytes());
    fp.push(1); // length in 16-byte paragraphs
    fp.push(4); // spec revision
    fp.push(0); // checksum
    fp.extend_from_slice(&[0; 5]); // feature bytes: config table present
    fp[10] = checksum(&fp);

    if table_addr + table.len() as u64 > 0xa0000 {
        return Err(Error::new("MP table does not fit below 640 KiB"));
    }
    mem.write(MPTABLE_ADDR, &fp)?;
    mem.write(table_addr, &table)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(b: &[u8]) -> u8 {
        b.iter().fold(0u8, |a, x| a.wrapping_add(*x))
    }

    #[test]
    fn tables_are_well_formed() {
        let m = GuestMemory::new(64 << 20).unwrap();
        write(&m, 4).unwrap();
        let mut fp = [0u8; 16];
        m.read(MPTABLE_ADDR, &mut fp).unwrap();
        assert_eq!(&fp[..4], b"_MP_");
        assert_eq!(sum(&fp), 0);
        let addr = u32::from_le_bytes([fp[4], fp[5], fp[6], fp[7]]) as u64;
        let mut h = [0u8; 44];
        m.read(addr, &mut h).unwrap();
        assert_eq!(&h[..4], b"PCMP");
        let len = u16::from_le_bytes([h[4], h[5]]) as usize;
        let mut table = vec![0u8; len];
        m.read(addr, &mut table).unwrap();
        assert_eq!(sum(&table), 0);
        let count = u16::from_le_bytes([h[34], h[35]]) as usize;
        // walk the entries: processors are 20 bytes, everything else 8
        let (mut off, mut cpus, mut seen) = (44, 0, 0);
        while off < len {
            let size = if table[off] == 0 { 20 } else { 8 };
            if table[off] == 0 {
                cpus += 1;
            }
            off += size;
            seen += 1;
        }
        assert_eq!(off, len);
        assert_eq!(seen, count);
        assert_eq!(cpus, 4);
    }
}
