// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! The hardware-virtualization backends. Each one maps guest memory, puts
//! the boot vCPU in the state [`crate::boot`] computed, and runs it, sending
//! port I/O and MMIO to [`crate::devices::Devices`].

use std::sync::Arc;

use crate::boot::BootState;
use crate::devices::Devices;
use crate::memory::GuestMemory;
use crate::{Outcome, Result};

#[cfg(target_os = "linux")]
pub mod kvm;
#[cfg(windows)]
pub mod whp;

/// Whether this platform needs bvmm to emulate the I/O APIC (KVM has one in
/// the kernel; WHP emulates only the local APIC).
pub const NEEDS_IOAPIC: bool = cfg!(windows);

/// The most virtual processors this platform's backend can run.
pub const MAX_CPUS: u8 = if cfg!(windows) { 1 } else { 64 };

/// Run the guest until it resets, halts or fails.
pub fn run(
    mem: &GuestMemory,
    boot: &BootState,
    devices: Arc<Devices>,
    cpus: u8,
) -> Result<Outcome> {
    #[cfg(target_os = "linux")]
    {
        kvm::run(mem, boot, devices, cpus)
    }
    #[cfg(windows)]
    {
        whp::run(mem, boot, devices, cpus)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (mem, boot, devices, cpus);
        Err(crate::Error::new(
            "bContain's hypervisor runs on Linux (KVM) and Windows (WHP)",
        ))
    }
}
