// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Virtio 1.x devices. A device implements [`VirtioDevice`]; the transport
//! in [`crate::devices::pci`] presents it to the guest on the PCI bus.
//! Written from the OASIS Virtio 1.2 specification.

pub mod blk;
pub mod queue;

use crate::memory::GuestMemory;
use queue::Queue;

/// Device type numbers from the specification (section 5).
pub const TYPE_BLOCK: u32 = 2;

/// `VIRTIO_F_VERSION_1`: the device follows the 1.x specification.
pub const F_VERSION_1: u64 = 1 << 32;

/// Device status bits the driver writes during initialization.
pub const STATUS_DRIVER_OK: u8 = 4;
pub const STATUS_FEATURES_OK: u8 = 8;

pub trait VirtioDevice: Send {
    /// One of the `TYPE_*` numbers.
    fn device_type(&self) -> u32;
    /// Device-specific feature bits offered (the transport adds `F_VERSION_1`).
    fn features(&self) -> u64;
    /// The largest size the guest may give each queue; one entry per queue.
    fn queue_max_sizes(&self) -> Vec<u16>;
    /// Read from the device-specific configuration space.
    fn read_config(&self, offset: u64, data: &mut [u8]);
    /// Write to the device-specific configuration space.
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    /// The driver finished negotiating; `features` is what it accepted.
    fn activate(&mut self, _features: u64) {}
    /// The driver kicked queue `index`. Returns true if buffers were
    /// returned to the guest, so it should be interrupted.
    fn notify(&mut self, index: usize, queue: &mut Queue, mem: &GuestMemory) -> bool;
    /// The driver reset the device.
    fn reset(&mut self) {}
}
