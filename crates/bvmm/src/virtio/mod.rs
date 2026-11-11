// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Virtio 1.x devices. A device implements [`VirtioDevice`]; the transport
//! in [`crate::devices::pci`] presents it to the guest on the PCI bus.
//! Written from the OASIS Virtio 1.2 specification.

pub mod blk;
pub mod console;
pub mod gpu;
pub mod input;
pub mod net;
pub mod queue;
pub mod rng;
pub mod vsock;

use crate::memory::GuestMemory;
use queue::Queue;

/// Device type numbers from the specification (section 5).
pub const TYPE_NET: u32 = 1;
pub const TYPE_BLOCK: u32 = 2;
pub const TYPE_CONSOLE: u32 = 3;
pub const TYPE_RNG: u32 = 4;
pub const TYPE_GPU: u32 = 16;
pub const TYPE_INPUT: u32 = 18;
pub const TYPE_VSOCK: u32 = 19;

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
    /// Work the device does on its own initiative (deliver input events,
    /// incoming data). Called when the host asked for it and after every
    /// kick. Returns the indexes of queues that got buffers back.
    fn poll(&mut self, _queues: &mut [Queue], _mem: &GuestMemory) -> Vec<usize> {
        Vec::new()
    }
    /// The driver reset the device.
    fn reset(&mut self) {}
}
