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

/// How the guest kernel should find its clock on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClockSource {
    /// Leave it to the hypervisor's paravirtual interface (Hyper-V
    /// enlightenments on WHP, kvm-clock on KVM), whatever the kernel supports.
    #[default]
    Paravirtual,
    /// On WHP: tell the kernel to ignore the hypervisor and give it the
    /// measured TSC frequency. Experimental and currently broken: see
    /// [`clock_kernel_params`].
    HostTsc,
}

/// Kernel parameters for the chosen clock.
///
/// On KVM nothing is needed: kernels find kvm-clock or calibrate the TSC
/// against KVM's PIT.
///
/// On WHP the guest is told it runs on Hyper-V. A kernel built with Hyper-V
/// support then uses Hyper-V's clock and all is well. A kernel without it
/// (Android's is one) still recognises Hyper-V, takes the timer frequencies
/// from it, but marks the TSC unstable because of Hyper-V and, having no
/// Hyper-V clock driver, ends up counting timer ticks: it runs, with a
/// coarse clock.
///
/// [`ClockSource::HostTsc`] was an attempt to fix that with
/// `nopv tsc_early_khz=N`. Tried on WHP on 2026-11-10 it made things worse:
/// the kernel then has the TSC but no reference to calibrate the local APIC
/// timer against ("APIC timer disabled due to verification failure"), so it
/// has no tick at all and hangs as soon as it needs one. It needs an emulated
/// PIT to become usable, and stays opt-in until then.
pub fn clock_kernel_params(clock: ClockSource) -> String {
    #[cfg(windows)]
    {
        if clock == ClockSource::HostTsc {
            if let Some(khz) = measure_tsc_khz() {
                return format!("nopv tsc_early_khz={khz}");
            }
        }
    }
    let _ = clock;
    String::new()
}

/// The host's TSC frequency in kHz, measured against the monotonic clock.
/// Good to roughly one part in ten thousand; a guest that needs better
/// should discipline its clock over the network, as real machines do.
#[cfg(any(windows, test))]
fn measure_tsc_khz() -> Option<u64> {
    #[cfg(target_arch = "x86_64")]
    {
        use std::time::{Duration, Instant};
        let sample = |window: Duration| -> Option<u64> {
            let t0 = Instant::now();
            // SAFETY: RDTSC is available on every x86-64 processor.
            let c0 = unsafe { core::arch::x86_64::_rdtsc() };
            while t0.elapsed() < window {
                std::hint::spin_loop();
            }
            let c1 = unsafe { core::arch::x86_64::_rdtsc() };
            let nanos = t0.elapsed().as_nanos() as u64;
            let ticks = c1.checked_sub(c0)?;
            (nanos > 0).then(|| (ticks as u128 * 1_000_000 / nanos as u128) as u64)
        };
        // Two windows that must agree, so a thread descheduled mid-sample
        // or a TSC that jumped is not believed.
        let a = sample(Duration::from_millis(60))?;
        let b = sample(Duration::from_millis(60))?;
        let plausible = (500_000..=10_000_000).contains(&a);
        let agree = a.abs_diff(b) * 2_000 < a;
        (plausible && agree).then_some((a + b) / 2)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsc_measurement_is_plausible_when_it_answers() {
        // On a busy or virtualized test host the two samples may disagree,
        // in which case no answer is the right answer.
        if let Some(khz) = measure_tsc_khz() {
            assert!((500_000..=10_000_000).contains(&khz), "{khz} kHz");
        }
    }

    #[test]
    fn paravirtual_clock_adds_no_parameters() {
        assert_eq!(clock_kernel_params(ClockSource::Paravirtual), "");
        assert_eq!(clock_kernel_params(ClockSource::default()), "");
        #[cfg(not(windows))]
        assert_eq!(clock_kernel_params(ClockSource::HostTsc), "");
    }
}
