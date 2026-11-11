// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! The guest's legacy PC devices and the port/MMIO bus that reaches them.

pub mod cmos;
pub mod ioapic;
pub mod pci;
pub mod serial;

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::layout::IOAPIC_ADDR;
use cmos::Cmos;
use ioapic::{Interrupt, IoApic};
use pci::PciBus;
use serial::Serial;

pub const COM1: u16 = 0x3f8;
pub const COM1_IRQ: u8 = 4;

/// How device interrupts reach the vCPU.
pub enum IrqRoute {
    /// The hypervisor has an in-kernel PIC + I/O APIC (KVM): pulse an ISA line.
    KernelLine(Box<dyn Fn(u8) + Send + Sync>),
    /// We emulate the I/O APIC (WHP) and hand the result to the local APIC.
    Ioapic(Box<dyn Fn(Interrupt) + Send + Sync>),
}

pub struct Devices {
    serial: Mutex<Serial>,
    cmos: Mutex<Cmos>,
    /// Present when the platform has no in-kernel I/O APIC.
    ioapic: Option<Mutex<IoApic>>,
    route: OnceLock<IrqRoute>,
    /// The PCI bus, when the machine has PCI devices.
    pci: Option<Mutex<PciBus>>,
    /// How an MSI message (address, data) reaches a local APIC.
    msi: OnceLock<Box<dyn Fn(u64, u32) + Send + Sync>>,
    reset: AtomicBool,
    /// How host memory is mapped into the guest; set by the hypervisor
    /// backend when it starts.
    mapper: crate::memory::MapperSlot,
}

impl Devices {
    pub fn new(console: Box<dyn Write + Send>, emulate_ioapic: bool, ioapic_id: u8) -> Devices {
        Devices {
            serial: Mutex::new(Serial::new(console)),
            cmos: Mutex::new(Cmos::default()),
            ioapic: emulate_ioapic.then(|| Mutex::new(IoApic::new(ioapic_id))),
            route: OnceLock::new(),
            pci: None,
            msi: OnceLock::new(),
            reset: AtomicBool::new(false),
            mapper: Default::default(),
        }
    }

    /// Give the machine a PCI bus.
    pub fn with_pci(mut self, bus: PciBus) -> Devices {
        self.pci = Some(Mutex::new(bus));
        self
    }

    /// Use `slot` for the machine's memory mapper, so that devices made
    /// before the machine could be given a clone of it.
    pub fn with_mapper_slot(mut self, slot: crate::memory::MapperSlot) -> Devices {
        self.mapper = slot;
        self
    }

    /// Called by the hypervisor backend once it can map memory.
    pub fn set_mapper(&self, mapper: Box<dyn crate::memory::Mapper>) {
        let _ = self.mapper.set(mapper);
    }

    pub fn has_pci(&self) -> bool {
        self.pci.is_some()
    }

    pub fn set_msi_route(&self, route: Box<dyn Fn(u64, u32) + Send + Sync>) {
        let _ = self.msi.set(route);
    }

    /// Deliver whatever the host has queued for the guest on PCI devices
    /// (input events and the like). Safe to call from any thread.
    pub fn poll_devices(&self) {
        if let Some(bus) = &self.pci {
            bus.lock().unwrap().poll(&|a, d| self.send_msi(a, d));
        }
    }

    fn send_msi(&self, address: u64, data: u32) {
        if let Some(f) = self.msi.get() {
            f(address, data);
        }
    }

    pub fn set_irq_route(&self, route: IrqRoute) {
        let _ = self.route.set(route);
    }

    /// The guest asked to reset (keyboard controller or port 0xcf9). Our
    /// container init reboots to exit, so this is "the VM is done".
    pub fn reset_requested(&self) -> bool {
        self.reset.load(Ordering::SeqCst)
    }

    fn raise_isa(&self, irq: u8) {
        match self.route.get() {
            Some(IrqRoute::KernelLine(f)) => f(irq),
            Some(IrqRoute::Ioapic(deliver)) => {
                let pin = if irq == 0 { 2 } else { irq as usize };
                let hit = self
                    .ioapic
                    .as_ref()
                    .and_then(|io| io.lock().unwrap().assert(pin));
                if let Some(i) = hit {
                    deliver(i);
                }
            }
            None => {}
        }
    }

    pub fn io_in(&self, port: u16, size: u8) -> u32 {
        let ones = if size >= 4 {
            u32::MAX
        } else {
            (1u32 << (size * 8)) - 1
        };
        match port {
            COM1..=0x3ff => {
                let (v, edge) = self.serial.lock().unwrap().read((port - COM1) as u8);
                if edge {
                    self.raise_isa(COM1_IRQ);
                }
                v as u32
            }
            0x71 => self.cmos.lock().unwrap().read_data() as u32,
            pci::CONFIG_ADDRESS..=0xcff if self.pci.is_some() => {
                self.pci.as_ref().unwrap().lock().unwrap().io_in(port, size)
            }
            // i8042: status "nothing to read", data 0.
            0x60 | 0x64 => 0,
            // No 8259 PIC behind an emulated I/O APIC: reading back the mask
            // the guest just wrote fails, so Linux uses its "null" PIC.
            0x20 | 0x21 | 0xa0 | 0xa1 if self.ioapic.is_some() => 0,
            _ => ones,
        }
    }

    pub fn io_out(&self, port: u16, size: u8, value: u32) {
        match port {
            COM1..=0x3ff => {
                let edge = self
                    .serial
                    .lock()
                    .unwrap()
                    .write((port - COM1) as u8, value as u8);
                if edge {
                    self.raise_isa(COM1_IRQ);
                }
            }
            0x70 => self.cmos.lock().unwrap().write_index(value as u8),
            // Keyboard-controller reset pulse, or the PCI reset control register.
            0x64 if value as u8 == 0xfe => self.reset.store(true, Ordering::SeqCst),
            // The reset control register is a byte at 0xcf9, inside the PCI
            // configuration address port; a byte write there is never PCI.
            0xcf9 if size == 1 => {
                if value & 0x04 != 0 {
                    self.reset.store(true, Ordering::SeqCst)
                }
            }
            pci::CONFIG_ADDRESS..=0xcff => {
                if let Some(bus) = &self.pci {
                    bus.lock()
                        .unwrap()
                        .io_out(port, size, value, &|a, d| self.send_msi(a, d));
                }
            }
            _ => {}
        }
    }

    pub fn mmio_read(&self, addr: u64, size: u8) -> u64 {
        match &self.ioapic {
            Some(io) if (IOAPIC_ADDR..IOAPIC_ADDR + 0x1000).contains(&addr) => {
                return io.lock().unwrap().mmio_read(addr - IOAPIC_ADDR) as u64;
            }
            _ => {}
        }
        self.pci
            .as_ref()
            .and_then(|bus| bus.lock().unwrap().mmio_read(addr, size))
            .unwrap_or(u64::MAX)
    }

    pub fn mmio_write(&self, addr: u64, size: u8, value: u64) {
        if let Some(io) = &self.ioapic {
            if (IOAPIC_ADDR..IOAPIC_ADDR + 0x1000).contains(&addr) {
                io.lock()
                    .unwrap()
                    .mmio_write(addr - IOAPIC_ADDR, value as u32);
                return;
            }
        }
        if let Some(bus) = &self.pci {
            bus.lock()
                .unwrap()
                .mmio_write(addr, size, value, &|a, d| self.send_msi(a, d));
        }
    }

    /// The local APIC acknowledged `vector` (level-triggered bookkeeping).
    pub fn eoi(&self, vector: u8) {
        if let Some(io) = &self.ioapic {
            io.lock().unwrap().eoi(vector);
        }
    }

    /// Keyboard input from the host console.
    pub fn console_input(&self, bytes: &[u8]) {
        let edge = self.serial.lock().unwrap().push_input(bytes);
        if edge {
            self.raise_isa(COM1_IRQ);
        }
    }

    pub fn flush(&self) {
        self.serial.lock().unwrap().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn serial_interrupt_reaches_ioapic_route() {
        let d = Devices::new(Box::new(std::io::sink()), true, 1);
        let got: Arc<Mutex<Vec<Interrupt>>> = Arc::default();
        let g = got.clone();
        d.set_irq_route(IrqRoute::Ioapic(Box::new(move |i| {
            g.lock().unwrap().push(i)
        })));
        // Guest programs I/O APIC pin 4 -> vector 0x34, then enables THRE interrupts.
        d.mmio_write(IOAPIC_ADDR, 4, 0x10 + 8);
        d.mmio_write(IOAPIC_ADDR + 0x10, 4, 0x34);
        d.io_out(COM1 + 1, 1, 0x02);
        assert_eq!(got.lock().unwrap().len(), 1);
        assert_eq!(got.lock().unwrap()[0].vector, 0x34);
    }

    #[test]
    fn reset_and_pic_probe() {
        let d = Devices::new(Box::new(std::io::sink()), true, 1);
        d.io_out(0x21, 1, 0xff);
        assert_ne!(d.io_in(0x21, 1), 0xff, "PIC must look absent");
        assert!(!d.reset_requested());
        d.io_out(0x64, 1, 0xfe);
        assert!(d.reset_requested());
        assert_eq!(d.io_in(0x1234, 2), 0xffff);
    }
}
