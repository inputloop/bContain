// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! A PCI bus with a host bridge and virtio devices on it.
//!
//! * Configuration access is mechanism #1: an address latch at I/O port
//!   0xcf8 and a data window at 0xcfc (PCI Local Bus 3.0, section 3.2.2.3.2).
//! * Each virtio device uses the modern virtio-pci transport (Virtio 1.2,
//!   section 4.1): its registers live in a memory BAR and are located through
//!   vendor capabilities.
//! * Interrupts are MSI-X only. A message is an (address, data) pair the
//!   platform turns into a local APIC interrupt, so no I/O APIC pin or
//!   interrupt routing table is involved.

use std::sync::Arc;

use super::ioapic::Interrupt;
use crate::memory::GuestMemory;
use crate::virtio::queue::Queue;
use crate::virtio::{VirtioDevice, F_VERSION_1, STATUS_DRIVER_OK};

pub const CONFIG_ADDRESS: u16 = 0xcf8;
pub const CONFIG_DATA: u16 = 0xcfc;
/// Slot 0 of bus 0 is the host bridge. The first 31 devices are function 0
/// of slots 1 to 31, as always; any more become functions 1 to 7 of slots
/// counted down from 31, so that adding devices never moves the ones a
/// guest may know by address (its system disk is 00:01.0).
pub const MAX_DEVICES: usize = 31 + 31 * 7;

/// Devices get their memory BARs here, 64 KiB apart, inside the 32-bit hole
/// above guest RAM. The guest may move them; the current value is honoured.
const BAR_BASE: u32 = 0xc000_0000;
const BAR_STRIDE: u32 = 0x1_0000;
const BAR0_SIZE: u32 = 0x4000;
const BAR1_SIZE: u32 = 0x1000;
/// Shared-memory windows are given out upward from here, or from just
/// above guest RAM if that reaches higher. 64 GiB is low enough that the
/// windows fit the 39-bit addresses of ordinary desktop and laptop
/// processors (512 GiB), which a start "far above everything" does not.
const SHM_BASE: u64 = 0x10_0000_0000;
/// Where the shared-memory capability goes in configuration space.
const SHM_CAP: usize = 0x94;

// Layout of BAR0.
const COMMON: u64 = 0x0000;
const COMMON_LEN: u64 = 0x38;
const ISR: u64 = 0x1000;
const DEVICE_CFG: u64 = 0x2000;
const NOTIFY: u64 = 0x3000;
const NOTIFY_MULTIPLIER: u32 = 4;
// Layout of BAR1.
const MSIX_PBA: u64 = 0x800;

const NO_VECTOR: u16 = 0xffff;
const COMMAND_MEMORY: u16 = 0x2;
const MSIX_ENABLE: u16 = 0x8000;
const MSIX_FUNCTION_MASK: u16 = 0x4000;

/// Where an MSI message goes.
pub type MsiSink<'a> = &'a dyn Fn(u64, u32);

/// Decode an MSI message (Intel SDM volume 3, section 11.11) into the
/// interrupt it asks the local APIC for.
pub fn decode_msi(address: u64, data: u32) -> Interrupt {
    Interrupt {
        vector: data as u8,
        delivery_mode: ((data >> 8) & 7) as u8,
        logical: address & 0x4 != 0,
        level: data & 0x8000 != 0,
        destination: (address >> 12) as u8,
    }
}

fn put16(cfg: &mut [u8; 256], at: usize, v: u16) {
    cfg[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(cfg: &mut [u8; 256], at: usize, v: u32) {
    cfg[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn get32(cfg: &[u8; 256], at: usize) -> u32 {
    u32::from_le_bytes(cfg[at..at + 4].try_into().unwrap())
}

/// An Intel 440FX-style host bridge: what the guest expects at 00:00.0.
fn host_bridge() -> [u8; 256] {
    let mut c = [0u8; 256];
    put16(&mut c, 0x00, 0x8086);
    put16(&mut c, 0x02, 0x1237);
    c[0x0b] = 0x06; // class: bridge, host
    c
}

#[derive(Clone, Copy, Default)]
struct MsixEntry {
    address: u64,
    data: u32,
    masked: bool,
    pending: bool,
}

/// One virtio device behind the modern virtio-pci transport.
pub struct VirtioPci {
    config: [u8; 256],
    command: u16,
    bars: [u32; 2],
    /// A shared-memory window in 64-bit BAR 2: its size, and the BAR's two
    /// registers as the guest last wrote them.
    shm: Option<(u64, [u32; 2])>,
    msix_control: u16,
    msix: Vec<MsixEntry>,
    dev: Box<dyn VirtioDevice>,
    queues: Vec<Queue>,
    device_feature_select: u32,
    driver_feature_select: u32,
    driver_features: u64,
    status: u8,
    queue_select: u16,
    msix_config: u16,
    isr: u8,
}

impl VirtioPci {
    /// `slot` numbers the device from 1; `next_window` is where the next
    /// shared-memory window may start, and is moved past any taken.
    fn new(slot: usize, dev: Box<dyn VirtioDevice>, next_window: &mut u64) -> VirtioPci {
        let kind = dev.device_type();
        let queues: Vec<Queue> = dev.queue_max_sizes().into_iter().map(Queue::new).collect();
        let vectors = queues.len() as u16 + 1;
        let mut c = [0u8; 256];
        put16(&mut c, 0x00, 0x1af4);
        put16(&mut c, 0x02, 0x1040 + kind as u16);
        put16(&mut c, 0x06, 0x0010); // status: has a capability list
        c[0x08] = 1; // revision: modern-only
        let class: [u8; 3] = match kind {
            crate::virtio::TYPE_BLOCK => [0x00, 0x80, 0x01], // mass storage, other
            crate::virtio::TYPE_GPU => [0x00, 0x80, 0x03],   // display, other
            crate::virtio::TYPE_INPUT => [0x00, 0x80, 0x09], // input, other
            _ => [0x00, 0x00, 0xff],
        };
        c[0x09..0x0c].copy_from_slice(&class);
        put16(&mut c, 0x2c, 0x1af4);
        put16(&mut c, 0x2e, 0x0040 + kind as u16);
        c[0x34] = 0x40; // first capability
                        // MSI-X: table in BAR1 at 0, pending bits in BAR1 at MSIX_PBA.
        c[0x40] = 0x11;
        c[0x41] = 0x50;
        put16(&mut c, 0x42, vectors - 1);
        put32(&mut c, 0x44, 1);
        put32(&mut c, 0x48, MSIX_PBA as u32 | 1);
        // Virtio structures, all in BAR0: (config offset, type, offset, length).
        let caps: [(usize, u8, u32, u32); 4] = [
            (0x50, 1, COMMON as u32, COMMON_LEN as u32),
            (0x60, 3, ISR as u32, 1),
            (0x70, 4, DEVICE_CFG as u32, 0x1000),
            (0x80, 2, NOTIFY as u32, 0x1000),
        ];
        for (i, (at, kind, offset, length)) in caps.iter().enumerate() {
            let last = i == caps.len() - 1;
            c[*at] = 0x09; // vendor-specific
            c[at + 1] = if last { 0 } else { caps[i + 1].0 as u8 };
            c[at + 2] = if last { 20 } else { 16 };
            c[at + 3] = *kind;
            c[at + 4] = 0; // BAR 0
            put32(&mut c, at + 8, *offset);
            put32(&mut c, at + 12, *length);
        }
        put32(&mut c, 0x80 + 16, NOTIFY_MULTIPLIER);
        let base = BAR_BASE + slot as u32 * BAR_STRIDE;
        let mut dev = dev;
        let shm = dev.shm_region().map(|(id, size)| {
            let size = size.next_power_of_two().max(0x1000);
            // A BAR sits at a multiple of its size.
            let gpa = next_window.div_ceil(size) * size;
            *next_window = gpa + size;
            // struct virtio_pci_cap64: the usual capability, then the high
            // halves of offset and length. Type 8: shared memory.
            c[0x80 + 1] = SHM_CAP as u8;
            c[SHM_CAP] = 0x09;
            c[SHM_CAP + 1] = 0;
            c[SHM_CAP + 2] = 24;
            c[SHM_CAP + 3] = 8;
            c[SHM_CAP + 4] = 2; // BAR 2
            c[SHM_CAP + 5] = id;
            put32(&mut c, SHM_CAP + 8, 0);
            put32(&mut c, SHM_CAP + 12, size as u32);
            put32(&mut c, SHM_CAP + 16, 0);
            put32(&mut c, SHM_CAP + 20, (size >> 32) as u32);
            dev.set_shm_base(gpa);
            (size, [gpa as u32, (gpa >> 32) as u32])
        });
        VirtioPci {
            config: c,
            command: 0,
            bars: [base, base + 0x8000],
            shm,
            msix_control: 0,
            msix: vec![
                MsixEntry {
                    masked: true,
                    ..Default::default()
                };
                vectors as usize
            ],
            dev,
            queues,
            device_feature_select: 0,
            driver_feature_select: 0,
            driver_features: 0,
            status: 0,
            queue_select: 0,
            msix_config: NO_VECTOR,
            isr: 0,
        }
    }

    fn config_read32(&self, reg: usize) -> u32 {
        match reg {
            0x04 => self.command as u32 | (get32(&self.config, 0x04) & 0xffff_0000),
            0x10 => self.bars[0],
            0x14 => self.bars[1],
            // Memory, 64-bit, prefetchable.
            0x18 if self.shm.is_some() => self.shm.map_or(0, |(_, r)| r[0]) | 0xc,
            0x1c if self.shm.is_some() => self.shm.map_or(0, |(_, r)| r[1]),
            0x40 => {
                let fixed = get32(&self.config, 0x40) & 0x3fff_ffff;
                fixed | (self.msix_control as u32) << 16
            }
            r if r + 4 <= 256 => get32(&self.config, r),
            _ => 0xffff_ffff,
        }
    }

    fn config_write32(&mut self, reg: usize, v: u32, sink: MsiSink) {
        match reg {
            0x04 => self.command = v as u16 & 0x0407,
            0x10 => self.bars[0] = v & !(BAR0_SIZE - 1),
            0x14 => self.bars[1] = v & !(BAR1_SIZE - 1),
            // The guest sizes the window by writing all ones and reading
            // back. Mappings are made at the address the window was given
            // at start, so the guest must leave it there; Linux does.
            0x18 => {
                if let Some((size, r)) = &mut self.shm {
                    r[0] = v & !((*size - 1) as u32) & !0xf;
                }
            }
            0x1c => {
                if let Some((size, r)) = &mut self.shm {
                    r[1] = v & !(((*size - 1) >> 32) as u32);
                }
            }
            0x40 => {
                let was_blocked = self.msix_blocked();
                self.msix_control = (v >> 16) as u16 & (MSIX_ENABLE | MSIX_FUNCTION_MASK);
                if was_blocked && !self.msix_blocked() {
                    for i in 0..self.msix.len() {
                        self.fire_if_pending(i, sink);
                    }
                }
            }
            _ => {}
        }
    }

    fn msix_blocked(&self) -> bool {
        self.msix_control & MSIX_ENABLE == 0 || self.msix_control & MSIX_FUNCTION_MASK != 0
    }

    fn fire_if_pending(&mut self, vector: usize, sink: MsiSink) {
        let blocked = self.msix_blocked();
        if let Some(e) = self.msix.get_mut(vector) {
            if e.pending && !e.masked && !blocked {
                e.pending = false;
                sink(e.address, e.data);
            }
        }
    }

    /// Tell the driver something happened on `vector` (a queue's, or the
    /// configuration vector).
    fn interrupt(&mut self, vector: u16, sink: MsiSink) {
        self.isr |= 1;
        if let Some(e) = self.msix.get_mut(vector as usize) {
            e.pending = true;
        }
        self.fire_if_pending(vector as usize, sink);
    }

    /// Which BAR holds `addr`, and the offset within it.
    fn bar_offset(&self, addr: u64) -> Option<(usize, u64)> {
        if self.command & COMMAND_MEMORY == 0 {
            return None;
        }
        let sizes = [BAR0_SIZE as u64, BAR1_SIZE as u64];
        (0..2).find_map(|i| {
            let base = self.bars[i] as u64;
            (base != 0 && addr >= base && addr < base + sizes[i]).then(|| (i, addr - base))
        })
    }

    fn accept_vector(&self, v: u16) -> u16 {
        if (v as usize) < self.msix.len() {
            v
        } else {
            NO_VECTOR
        }
    }

    fn selected(&mut self) -> Option<&mut Queue> {
        self.queues.get_mut(self.queue_select as usize)
    }

    /// The common configuration structure as bytes (Virtio 1.2, 4.1.4.3).
    fn common_snapshot(&self) -> [u8; COMMON_LEN as usize] {
        let mut b = [0u8; COMMON_LEN as usize];
        let offered = self.dev.features() | F_VERSION_1;
        let pick = |all: u64, select: u32| -> u32 {
            match select {
                0 => all as u32,
                1 => (all >> 32) as u32,
                _ => 0,
            }
        };
        b[0x00..0x04].copy_from_slice(&self.device_feature_select.to_le_bytes());
        b[0x04..0x08].copy_from_slice(&pick(offered, self.device_feature_select).to_le_bytes());
        b[0x08..0x0c].copy_from_slice(&self.driver_feature_select.to_le_bytes());
        b[0x0c..0x10]
            .copy_from_slice(&pick(self.driver_features, self.driver_feature_select).to_le_bytes());
        b[0x10..0x12].copy_from_slice(&self.msix_config.to_le_bytes());
        b[0x12..0x14].copy_from_slice(&(self.queues.len() as u16).to_le_bytes());
        b[0x14] = self.status;
        b[0x15] = 0; // config generation: the configuration never changes
        b[0x16..0x18].copy_from_slice(&self.queue_select.to_le_bytes());
        if let Some(q) = self.queues.get(self.queue_select as usize) {
            b[0x18..0x1a].copy_from_slice(&q.size.to_le_bytes());
            b[0x1a..0x1c].copy_from_slice(&q.msix_vector.to_le_bytes());
            b[0x1c..0x1e].copy_from_slice(&(q.ready as u16).to_le_bytes());
            b[0x1e..0x20].copy_from_slice(&self.queue_select.to_le_bytes());
            b[0x20..0x28].copy_from_slice(&q.desc.to_le_bytes());
            b[0x28..0x30].copy_from_slice(&q.avail.to_le_bytes());
            b[0x30..0x38].copy_from_slice(&q.used.to_le_bytes());
        }
        b
    }

    fn common_write(&mut self, offset: u64, size: u8, v: u64) {
        fn low(old: u64, v: u64) -> u64 {
            (old & !0xffff_ffff) | (v & 0xffff_ffff)
        }
        fn high(old: u64, v: u64) -> u64 {
            (old & 0xffff_ffff) | (v & 0xffff_ffff) << 32
        }
        if size == 8 && matches!(offset, 0x20 | 0x28 | 0x30) {
            self.common_write(offset, 4, v & 0xffff_ffff);
            self.common_write(offset + 4, 4, v >> 32);
            return;
        }
        match offset {
            0x00 => self.device_feature_select = v as u32,
            0x08 => self.driver_feature_select = v as u32,
            0x0c => match self.driver_feature_select {
                0 => self.driver_features = low(self.driver_features, v),
                1 => self.driver_features = high(self.driver_features, v),
                _ => {}
            },
            0x10 => self.msix_config = self.accept_vector(v as u16),
            0x14 => self.set_status(v as u8),
            0x16 => self.queue_select = v as u16,
            0x18 => {
                if let Some(q) = self.selected() {
                    q.size = v as u16;
                }
            }
            0x1a => {
                let vector = self.accept_vector(v as u16);
                if let Some(q) = self.selected() {
                    q.msix_vector = vector;
                }
            }
            0x1c => {
                if let Some(q) = self.selected() {
                    q.ready = v & 1 != 0;
                }
            }
            0x20 | 0x24 | 0x28 | 0x2c | 0x30 | 0x34 => {
                let Some(q) = self.selected() else { return };
                let field = match offset & !4 {
                    0x20 => &mut q.desc,
                    0x28 => &mut q.avail,
                    _ => &mut q.used,
                };
                *field = if offset & 4 == 0 {
                    low(*field, v)
                } else {
                    high(*field, v)
                };
            }
            _ => {}
        }
    }

    fn set_status(&mut self, status: u8) {
        if status == 0 {
            for q in &mut self.queues {
                q.reset();
            }
            self.driver_features = 0;
            self.device_feature_select = 0;
            self.driver_feature_select = 0;
            self.queue_select = 0;
            self.msix_config = NO_VECTOR;
            self.isr = 0;
            self.status = 0;
            self.dev.reset();
            return;
        }
        let starting = status & STATUS_DRIVER_OK != 0 && self.status & STATUS_DRIVER_OK == 0;
        self.status = status;
        if starting {
            // Only features that were offered count as accepted.
            let accepted = self.driver_features & (self.dev.features() | F_VERSION_1);
            self.dev.activate(accepted);
        }
    }

    /// Let the device do work of its own and interrupt for what it returned.
    fn poll(&mut self, mem: &GuestMemory, sink: MsiSink) {
        if self.status & STATUS_DRIVER_OK == 0 {
            return;
        }
        for index in self.dev.poll(&mut self.queues, mem) {
            if let Some(vector) = self.queues.get(index).map(|q| q.msix_vector) {
                self.interrupt(vector, sink);
            }
        }
    }

    fn mmio_read(&mut self, bar: usize, offset: u64, size: u8) -> u64 {
        let n = (size as usize).min(8);
        let mut out = [0u8; 8];
        if bar == 1 {
            let (index, field) = ((offset / 16) as usize, offset % 16);
            if offset >= MSIX_PBA {
                let bits = self
                    .msix
                    .iter()
                    .enumerate()
                    .fold(0u64, |a, (i, e)| a | (e.pending as u64) << (i % 64));
                out = bits.to_le_bytes();
            } else if let Some(e) = self.msix.get(index) {
                let mut raw = [0u8; 16];
                raw[0..8].copy_from_slice(&e.address.to_le_bytes());
                raw[8..12].copy_from_slice(&e.data.to_le_bytes());
                raw[12..16].copy_from_slice(&(e.masked as u32).to_le_bytes());
                for (i, o) in out.iter_mut().take(n).enumerate() {
                    *o = raw.get(field as usize + i).copied().unwrap_or(0);
                }
            }
            return u64::from_le_bytes(out);
        }
        match offset {
            o if o < COMMON + COMMON_LEN => {
                let snap = self.common_snapshot();
                for (i, b) in out.iter_mut().take(n).enumerate() {
                    *b = snap.get(o as usize + i).copied().unwrap_or(0);
                }
            }
            ISR => {
                // Reading the interrupt status clears it.
                out[0] = std::mem::take(&mut self.isr);
            }
            o if (DEVICE_CFG..NOTIFY).contains(&o) => {
                self.dev.read_config(o - DEVICE_CFG, &mut out[..n]);
            }
            _ => {}
        }
        u64::from_le_bytes(out)
    }

    fn mmio_write(
        &mut self,
        bar: usize,
        offset: u64,
        size: u8,
        v: u64,
        mem: &GuestMemory,
        sink: MsiSink,
    ) {
        if bar == 1 {
            let (index, field) = ((offset / 16) as usize, offset % 16);
            if offset >= MSIX_PBA {
                return;
            }
            if let Some(e) = self.msix.get_mut(index) {
                match (field, size) {
                    (0, 8) => e.address = v,
                    (0, _) => e.address = (e.address & !0xffff_ffff) | (v & 0xffff_ffff),
                    (4, _) => e.address = (e.address & 0xffff_ffff) | (v & 0xffff_ffff) << 32,
                    (8, _) => e.data = v as u32,
                    (12, _) => e.masked = v & 1 != 0,
                    _ => {}
                }
            }
            self.fire_if_pending(index, sink);
            return;
        }
        match offset {
            o if o < COMMON + COMMON_LEN => self.common_write(o, size, v),
            o if (DEVICE_CFG..NOTIFY).contains(&o) => {
                let bytes = v.to_le_bytes();
                self.dev
                    .write_config(o - DEVICE_CFG, &bytes[..(size as usize).min(8)]);
            }
            o if o >= NOTIFY => {
                let index = ((o - NOTIFY) / NOTIFY_MULTIPLIER as u64) as usize;
                if self.status & STATUS_DRIVER_OK == 0 {
                    return;
                }
                let Some(queue) = self.queues.get_mut(index) else {
                    return;
                };
                if self.dev.notify(index, queue, mem) {
                    let vector = queue.msix_vector;
                    self.interrupt(vector, sink);
                }
                // The kick may have brought buffers the device was waiting for.
                self.poll(mem, sink);
            }
            _ => {}
        }
    }
}

pub struct PciBus {
    address: u32,
    bridge: [u8; 256],
    devices: Vec<VirtioPci>,
    mem: Arc<GuestMemory>,
    next_window: u64,
}

/// Which device (numbered from 1; 0 is the host bridge) is at a slot and
/// function. See [`MAX_DEVICES`].
fn device_at(slot: usize, func: usize) -> usize {
    if func == 0 {
        slot
    } else {
        32 + (31 - slot) * 7 + (func - 1)
    }
}

impl PciBus {
    pub fn new(mem: Arc<GuestMemory>) -> PciBus {
        PciBus {
            address: 0,
            bridge: host_bridge(),
            devices: Vec::new(),
            next_window: SHM_BASE.max(crate::layout::HIGH_MEMORY_START + mem.high_size()),
            mem,
        }
    }

    /// Put a virtio device in the next free slot (00:01.0 onwards).
    pub fn add_virtio(&mut self, dev: Box<dyn VirtioDevice>) {
        let slot = self.devices.len() + 1;
        let device = VirtioPci::new(slot, dev, &mut self.next_window);
        self.devices.push(device);
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// The register the address latch points at: (device, register), if it
    /// names a device on bus 0. Device 0 is the host bridge.
    fn target(&self) -> Option<(usize, usize)> {
        let a = self.address;
        let (bus, slot, func) = ((a >> 16) & 0xff, (a >> 11) & 0x1f, (a >> 8) & 7);
        if a & 0x8000_0000 == 0 || bus != 0 || (slot == 0 && func != 0) {
            return None;
        }
        let device = device_at(slot as usize, func as usize);
        (device <= self.devices.len()).then_some((device, (a & 0xfc) as usize))
    }

    fn read32(&self, slot: usize, reg: usize) -> u32 {
        match slot {
            0 => get32(&self.bridge, reg),
            s => {
                let mut value = self.devices[s - 1].config_read32(reg);
                // Header type, bit 7: "this slot has more functions". Set
                // on function 0 of a slot whose other functions are used,
                // or the guest would not look for them.
                if reg == 0x0c && s <= 31 && device_at(s, 1) <= self.devices.len() {
                    value |= 0x80 << 16;
                }
                value
            }
        }
    }

    /// Port read in 0xcf8..=0xcff.
    pub fn io_in(&self, port: u16, size: u8) -> u32 {
        let ones = if size >= 4 {
            u32::MAX
        } else {
            (1u32 << (size * 8)) - 1
        };
        if port == CONFIG_ADDRESS && size == 4 {
            return self.address;
        }
        if port < CONFIG_DATA {
            return ones;
        }
        match self.target() {
            Some((slot, reg)) => (self.read32(slot, reg) >> ((port - CONFIG_DATA) * 8)) & ones,
            None => ones,
        }
    }

    /// Port write in 0xcf8..=0xcff.
    pub fn io_out(&mut self, port: u16, size: u8, value: u32, sink: MsiSink) {
        if port == CONFIG_ADDRESS && size == 4 {
            self.address = value;
            return;
        }
        if port < CONFIG_DATA {
            return;
        }
        let Some((slot, reg)) = self.target() else {
            return;
        };
        if slot == 0 {
            return; // the bridge has nothing writable
        }
        // Merge a 1- or 2-byte write into the 32-bit register it lands in.
        let shift = (port - CONFIG_DATA) as u32 * 8;
        let mask = if size >= 4 {
            u32::MAX
        } else {
            ((1u32 << (size * 8)) - 1) << shift
        };
        let dev = &mut self.devices[slot - 1];
        let merged = (dev.config_read32(reg) & !mask) | ((value << shift) & mask);
        dev.config_write32(reg, merged, sink);
    }

    /// Let every device deliver what the host has queued for the guest.
    pub fn poll(&mut self, sink: MsiSink) {
        let mem = self.mem.clone();
        for d in &mut self.devices {
            d.poll(&mem, sink);
        }
    }

    /// Guest read of device memory. `None` if no BAR is at `addr`.
    pub fn mmio_read(&mut self, addr: u64, size: u8) -> Option<u64> {
        self.devices.iter_mut().find_map(|d| {
            d.bar_offset(addr)
                .map(|(bar, off)| d.mmio_read(bar, off, size))
        })
    }

    /// Guest write of device memory. False if no BAR is at `addr`.
    pub fn mmio_write(&mut self, addr: u64, size: u8, value: u64, sink: MsiSink) -> bool {
        let mem = self.mem.clone();
        for d in &mut self.devices {
            if let Some((bar, off)) = d.bar_offset(addr) {
                d.mmio_write(bar, off, size, value, &mem, sink);
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::queue::tests::{publish, put_desc, AVAIL, DESC, USED};
    use std::cell::RefCell;

    /// A device with one queue that returns every buffer untouched.
    struct Echo {
        activated: Option<u64>,
    }

    impl VirtioDevice for Echo {
        fn device_type(&self) -> u32 {
            crate::virtio::TYPE_BLOCK
        }
        fn features(&self) -> u64 {
            1 << 9
        }
        fn queue_max_sizes(&self) -> Vec<u16> {
            vec![64]
        }
        fn read_config(&self, offset: u64, data: &mut [u8]) {
            for (i, b) in data.iter_mut().enumerate() {
                *b = 0xa0 + offset as u8 + i as u8;
            }
        }
        fn activate(&mut self, features: u64) {
            self.activated = Some(features);
        }
        fn notify(&mut self, _i: usize, q: &mut Queue, mem: &GuestMemory) -> bool {
            let mut any = false;
            while let Ok(Some(c)) = q.pop(mem) {
                q.push_used(mem, c.head, 0).unwrap();
                any = true;
            }
            any
        }
    }

    fn bus() -> PciBus {
        let mut bus = PciBus::new(Arc::new(GuestMemory::new(64 << 20).unwrap()));
        bus.add_virtio(Box::new(Echo { activated: None }));
        bus
    }

    fn cfg_read(bus: &mut PciBus, slot: u32, reg: u32) -> u32 {
        bus.io_out(
            CONFIG_ADDRESS,
            4,
            0x8000_0000 | slot << 11 | reg,
            &|_, _| {},
        );
        bus.io_in(CONFIG_DATA, 4)
    }

    fn cfg_write(bus: &mut PciBus, slot: u32, reg: u32, v: u32) {
        bus.io_out(
            CONFIG_ADDRESS,
            4,
            0x8000_0000 | slot << 11 | reg,
            &|_, _| {},
        );
        bus.io_out(CONFIG_DATA, 4, v, &|_, _| {});
    }

    #[test]
    fn linux_type1_probe_and_enumeration() {
        let mut b = bus();
        // arch/x86/pci/direct.c: the latch must read back what was written.
        b.io_out(0xcfb, 1, 0x01, &|_, _| {});
        b.io_out(CONFIG_ADDRESS, 4, 0x8000_0000, &|_, _| {});
        assert_eq!(b.io_in(CONFIG_ADDRESS, 4), 0x8000_0000);
        assert_eq!(cfg_read(&mut b, 0, 0x00), 0x1237_8086, "host bridge");
        assert_eq!(cfg_read(&mut b, 0, 0x08) >> 16, 0x0600, "bridge class");
        assert_eq!(cfg_read(&mut b, 1, 0x00), 0x1042_1af4, "virtio-blk, modern");
        assert_eq!(cfg_read(&mut b, 1, 0x08), 0x0180_0001, "class and revision");
        assert_eq!(cfg_read(&mut b, 2, 0x00), 0xffff_ffff, "empty slot");
        // Function 1 of slot 1 does not exist.
        b.io_out(
            CONFIG_ADDRESS,
            4,
            0x8000_0000 | 1 << 11 | 1 << 8,
            &|_, _| {},
        );
        assert_eq!(b.io_in(CONFIG_DATA, 4), 0xffff_ffff);
        // Byte and word reads see the right part of the register.
        b.io_out(CONFIG_ADDRESS, 4, 0x8000_0000 | 1 << 11, &|_, _| {});
        assert_eq!(b.io_in(CONFIG_DATA + 2, 2), 0x1042);
        assert_eq!(b.io_in(CONFIG_DATA + 1, 1), 0x1a);
    }

    #[test]
    fn bars_size_and_move() {
        let mut b = bus();
        let original = cfg_read(&mut b, 1, 0x10);
        assert_eq!(original, BAR_BASE + BAR_STRIDE);
        cfg_write(&mut b, 1, 0x10, 0xffff_ffff);
        assert_eq!(cfg_read(&mut b, 1, 0x10), !(BAR0_SIZE - 1), "16 KiB");
        cfg_write(&mut b, 1, 0x14, 0xffff_ffff);
        assert_eq!(cfg_read(&mut b, 1, 0x14), !(BAR1_SIZE - 1), "4 KiB");
        cfg_write(&mut b, 1, 0x10, 0xd000_0000);
        // Memory decoding is off until the command register enables it.
        assert_eq!(b.mmio_read(0xd000_0000 + 0x12, 2), None);
        // A 2-byte write to the command register leaves the status half alone.
        b.io_out(CONFIG_ADDRESS, 4, 0x8000_0000 | 1 << 11 | 0x04, &|_, _| {});
        b.io_out(CONFIG_DATA, 2, 0x0002, &|_, _| {});
        assert_eq!(cfg_read(&mut b, 1, 0x04), 0x0010_0002);
        assert_eq!(b.mmio_read(0xd000_0000 + 0x12, 2), Some(1), "one queue");
        assert_eq!(b.mmio_read(original as u64 + 0x12, 2), None, "old address");
    }

    #[test]
    fn capabilities_describe_the_layout() {
        let mut b = bus();
        assert_eq!(cfg_read(&mut b, 1, 0x34) & 0xff, 0x40);
        let mut at = 0x40;
        let mut seen = Vec::new();
        while at != 0 {
            let head = cfg_read(&mut b, 1, at);
            if head & 0xff == 0x09 {
                let kind = head >> 24;
                let offset = cfg_read(&mut b, 1, at + 8);
                seen.push((kind, offset));
            } else {
                assert_eq!(head & 0xff, 0x11, "MSI-X");
                assert_eq!((head >> 16) & 0x7ff, 1, "two vectors");
            }
            at = (head >> 8) & 0xff;
        }
        assert_eq!(seen, vec![(1, 0), (3, 0x1000), (4, 0x2000), (2, 0x3000)]);
        assert_eq!(cfg_read(&mut b, 1, 0x90), NOTIFY_MULTIPLIER);
    }

    /// A device that only has a shared-memory window, and notes where it
    /// was told the window is.
    struct Windowed(Arc<std::sync::Mutex<u64>>);

    impl VirtioDevice for Windowed {
        fn device_type(&self) -> u32 {
            crate::virtio::TYPE_GPU
        }
        fn features(&self) -> u64 {
            0
        }
        fn queue_max_sizes(&self) -> Vec<u16> {
            vec![64]
        }
        fn read_config(&self, _: u64, data: &mut [u8]) {
            data.fill(0);
        }
        fn notify(&mut self, _: usize, _: &mut Queue, _: &GuestMemory) -> bool {
            false
        }
        fn shm_region(&self) -> Option<(u8, u64)> {
            Some((1, 8 << 30))
        }
        fn set_shm_base(&mut self, gpa: u64) {
            *self.0.lock().unwrap() = gpa;
        }
    }

    #[test]
    fn a_shared_memory_window_is_a_64_bit_bar_with_its_capability() {
        let told = Arc::new(std::sync::Mutex::new(0u64));
        let mut b = bus();
        b.add_virtio(Box::new(Windowed(told.clone()))); // slot 2
        let base = SHM_BASE; // 64 GiB: above this test's RAM, a multiple of 8 GiB
        assert_eq!(*told.lock().unwrap(), base, "the device knows its window");
        // BAR 2: memory, 64-bit, prefetchable, at the window's address.
        assert_eq!(cfg_read(&mut b, 2, 0x18), base as u32 | 0xc);
        assert_eq!(cfg_read(&mut b, 2, 0x1c), (base >> 32) as u32);
        // Sizing: all ones in, the size mask out; then the address goes back.
        cfg_write(&mut b, 2, 0x18, 0xffff_ffff);
        cfg_write(&mut b, 2, 0x1c, 0xffff_ffff);
        let mask =
            (cfg_read(&mut b, 2, 0x1c) as u64) << 32 | (cfg_read(&mut b, 2, 0x18) & !0xf) as u64;
        assert_eq!(!mask + 1, 8 << 30, "8 GiB");
        cfg_write(&mut b, 2, 0x18, base as u32);
        cfg_write(&mut b, 2, 0x1c, (base >> 32) as u32);
        assert_eq!(cfg_read(&mut b, 2, 0x1c), (base >> 32) as u32);
        // The capability chain ends with type 8 naming BAR 2, id 1, 8 GiB.
        let mut at = 0x40;
        let mut last = 0;
        while at != 0 {
            last = at;
            at = (cfg_read(&mut b, 2, at) >> 8) & 0xff;
        }
        let head = cfg_read(&mut b, 2, last);
        assert_eq!(
            (head & 0xff, (head >> 16) & 0xff, head >> 24),
            (0x09, 24, 8)
        );
        assert_eq!(
            cfg_read(&mut b, 2, last + 4) & 0xffff,
            0x0102,
            "BAR 2, id 1"
        );
        let length =
            (cfg_read(&mut b, 2, last + 20) as u64) << 32 | cfg_read(&mut b, 2, last + 12) as u64;
        assert_eq!(length, 8 << 30);
        // A device without a window has no such BAR or capability.
        assert_eq!(cfg_read(&mut b, 1, 0x18), 0);
    }

    #[test]
    fn more_than_31_devices_become_extra_functions_without_moving_the_first() {
        let mut b = bus(); // one device, at 00:01.0
        let id = |b: &mut PciBus, slot: u32, func: u32, reg: u32| {
            b.io_out(
                CONFIG_ADDRESS,
                4,
                0x8000_0000 | slot << 11 | func << 8 | reg,
                &|_, _| {},
            );
            b.io_in(CONFIG_DATA, 4)
        };
        let before = id(&mut b, 1, 0, 0);
        for _ in 1..33 {
            b.add_virtio(Box::new(Echo { activated: None }));
        }
        // 33 devices: 31 at function 0, then 31.1 and 31.2.
        assert_eq!(
            id(&mut b, 1, 0, 0),
            before,
            "the first device has not moved"
        );
        assert_eq!(id(&mut b, 31, 0, 0) & 0xffff, 0x1af4);
        assert_eq!(
            id(&mut b, 31, 0, 0x0c) >> 16 & 0x80,
            0x80,
            "slot 31 says it has more functions"
        );
        assert_eq!(
            id(&mut b, 30, 0, 0x0c) >> 16 & 0x80,
            0,
            "slot 30 has only one"
        );
        assert_eq!(id(&mut b, 31, 1, 0) & 0xffff, 0x1af4);
        assert_eq!(id(&mut b, 31, 2, 0) & 0xffff, 0x1af4);
        assert_eq!(id(&mut b, 31, 3, 0), 0xffff_ffff, "nothing there");
        assert_eq!(id(&mut b, 30, 1, 0), 0xffff_ffff);
        assert_eq!(
            id(&mut b, 0, 1, 0),
            0xffff_ffff,
            "the bridge has one function"
        );
        // Each has its own registers.
        let (a, c) = (id(&mut b, 31, 0, 0x10), id(&mut b, 31, 1, 0x10));
        assert_ne!(a, c);
        assert_eq!(
            (MAX_DEVICES, device_at(31, 1), device_at(1, 7)),
            (248, 32, 248)
        );
    }

    #[test]
    fn driver_handshake_then_a_request_raises_msix() {
        let mut b = bus();
        let base = cfg_read(&mut b, 1, 0x10) as u64;
        let msix = cfg_read(&mut b, 1, 0x14) as u64;
        cfg_write(&mut b, 1, 0x04, 0x0006);
        let fired: RefCell<Vec<(u64, u32)>> = RefCell::default();
        let sink = |a: u64, d: u32| fired.borrow_mut().push((a, d));
        let w = |b: &mut PciBus, off: u64, size: u8, v: u64| {
            assert!(b.mmio_write(base + off, size, v, &sink))
        };

        w(&mut b, 0x14, 1, 0); // reset
        w(&mut b, 0x14, 1, 1 | 2); // acknowledge, driver
        w(&mut b, 0x00, 4, 1);
        assert_eq!(
            b.mmio_read(base + 0x04, 4),
            Some(1),
            "VERSION_1 in the high word"
        );
        w(&mut b, 0x00, 4, 0);
        assert_eq!(b.mmio_read(base + 0x04, 4), Some(1 << 9));
        w(&mut b, 0x08, 4, 1);
        w(&mut b, 0x0c, 4, 1);
        w(&mut b, 0x08, 4, 0);
        w(&mut b, 0x0c, 4, 1 << 9 | 1 << 3); // bit 3 was never offered
        w(&mut b, 0x14, 1, 1 | 2 | 8);
        assert_eq!(b.mmio_read(base + 0x14, 1), Some(1 | 2 | 8));

        w(&mut b, 0x10, 2, 0); // configuration changes on vector 0
        w(&mut b, 0x16, 2, 0);
        assert_eq!(b.mmio_read(base + 0x18, 2), Some(64), "maximum queue size");
        w(&mut b, 0x18, 2, 8);
        w(&mut b, 0x1a, 2, 1);
        assert_eq!(b.mmio_read(base + 0x1a, 2), Some(1));
        w(&mut b, 0x1a, 2, 9);
        assert_eq!(b.mmio_read(base + 0x1a, 2), Some(0xffff), "no such vector");
        w(&mut b, 0x1a, 2, 1);
        w(&mut b, 0x20, 4, DESC);
        w(&mut b, 0x24, 4, 0);
        w(&mut b, 0x28, 4, AVAIL);
        w(&mut b, 0x2c, 4, 0);
        w(&mut b, 0x30, 4, USED);
        w(&mut b, 0x34, 4, 0);
        w(&mut b, 0x1c, 2, 1);
        assert_eq!(b.mmio_read(base + 0x20, 4), Some(DESC));
        assert_eq!(b.mmio_read(base + 0x2000, 2), Some(0xa1a0), "device config");

        // MSI-X vector 1 -> local APIC 0, vector 0x41. Left masked for now.
        assert!(b.mmio_write(msix + 16, 4, 0xfee0_0000, &sink));
        assert!(b.mmio_write(msix + 16 + 8, 4, 0x41, &sink));
        cfg_write(&mut b, 1, 0x40, (MSIX_ENABLE as u32) << 16);
        assert_eq!(cfg_read(&mut b, 1, 0x40) >> 16, 0x8001);

        let mem = b.mem.clone();
        put_desc(&mem, 0, 0x8000, 4, 0, 0);
        publish(&mem, 8, 0, 0);
        w(&mut b, 0x3000, 2, 0);
        assert_eq!(
            mem.read_u16(USED + 2).unwrap(),
            0,
            "ignored before DRIVER_OK"
        );
        w(&mut b, 0x14, 1, 1 | 2 | 8 | 4);
        w(&mut b, 0x3000, 2, 0);
        assert_eq!(mem.read_u16(USED + 2).unwrap(), 1, "request served");
        assert!(fired.borrow().is_empty(), "vector is still masked");
        assert_eq!(b.mmio_read(msix + MSIX_PBA, 8), Some(0b10), "pending bit");
        assert!(b.mmio_write(msix + 16 + 12, 4, 0, &sink));
        assert_eq!(*fired.borrow(), vec![(0xfee0_0000, 0x41)]);
        assert_eq!(b.mmio_read(msix + MSIX_PBA, 8), Some(0));
        assert_eq!(b.mmio_read(base + 0x1000, 1), Some(1), "ISR set");
        assert_eq!(
            b.mmio_read(base + 0x1000, 1),
            Some(0),
            "and cleared by the read"
        );

        // A second request interrupts straight away.
        publish(&mem, 8, 1, 0);
        w(&mut b, 0x3000, 2, 0);
        assert_eq!(fired.borrow().len(), 2);

        // Reset drops the queue setup.
        w(&mut b, 0x14, 1, 0);
        assert_eq!(b.mmio_read(base + 0x1c, 2), Some(0));
        assert_eq!(b.mmio_read(base + 0x10, 2), Some(0xffff));
    }

    #[test]
    fn msi_messages_decode() {
        let i = decode_msi(0xfee0_3000, 0x0000_4041);
        assert_eq!(
            (i.vector, i.destination, i.logical, i.level),
            (0x41, 3, false, false)
        );
        assert_eq!(i.delivery_mode, 0);
        let j = decode_msi(0xfee0_100c, 0x0000_8131);
        assert_eq!(
            (j.vector, j.destination, j.logical, j.level),
            (0x31, 1, true, true)
        );
        assert_eq!(j.delivery_mode, 1, "lowest priority");
    }
}
