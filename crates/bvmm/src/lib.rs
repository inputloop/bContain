// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! bvmm: the native-VMM seam for bContain.
//!
//! Today bContain drives QEMU. This crate is where bContain's *own*
//! hypervisor will live, so the rest of the codebase already talks to the
//! hardware-acceleration layer through types defined here:
//!
//! * [`probe`] answers "which hardware accelerator can this host use?"
//!   (KVM on Linux, Windows Hypervisor Platform on Windows). The QEMU backend
//!   uses the same answer to pick `-accel kvm` / `-accel whpx`.
//! * [`NativeVmm`] is the future in-process VMM. It is deliberately a stub
//!   that reports itself unavailable; see `docs/NATIVE-VMM.md` for the
//!   milestone plan (vCPU loop, memory slots, virtio-mmio devices, then
//!   direct kernel boot for container micro-VMs).

use std::fmt;

/// A hardware virtualization accelerator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accel {
    /// Linux Kernel-based Virtual Machine (`/dev/kvm`).
    Kvm,
    /// Windows Hypervisor Platform (`WinHvPlatform.dll`).
    Whp,
    /// No accelerator: pure software emulation.
    None,
}

impl Accel {
    /// The value QEMU expects after `-accel`.
    pub fn qemu_name(self) -> &'static str {
        match self {
            Accel::Kvm => "kvm",
            Accel::Whp => "whpx",
            Accel::None => "tcg",
        }
    }
}

impl fmt::Display for Accel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Accel::Kvm => "KVM",
            Accel::Whp => "Windows Hypervisor Platform",
            Accel::None => "none (software emulation)",
        })
    }
}

/// Result of probing the host for a usable accelerator.
#[derive(Debug, Clone)]
pub struct AccelProbe {
    pub accel: Accel,
    pub usable: bool,
    /// Human-readable explanation, including how to fix an unusable accelerator.
    pub detail: String,
}

/// Probe the host for the best available accelerator.
pub fn probe() -> AccelProbe {
    #[cfg(target_os = "linux")]
    {
        probe_kvm()
    }
    #[cfg(windows)]
    {
        probe_whp()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        AccelProbe {
            accel: Accel::None,
            usable: false,
            detail: "unsupported host OS; bContain targets Windows and Linux".into(),
        }
    }
}

#[cfg(target_os = "linux")]
fn probe_kvm() -> AccelProbe {
    use std::fs::OpenOptions;
    match OpenOptions::new().read(true).write(true).open("/dev/kvm") {
        Ok(_) => AccelProbe {
            accel: Accel::Kvm,
            usable: true,
            detail: "/dev/kvm is present and writable".into(),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => AccelProbe {
            accel: Accel::Kvm,
            usable: false,
            detail: "/dev/kvm not found: enable VT-x in firmware and load kvm_intel \
                     (`sudo modprobe kvm_intel`)"
                .into(),
        },
        Err(e) => AccelProbe {
            accel: Accel::Kvm,
            usable: false,
            detail: format!(
                "/dev/kvm exists but cannot be opened ({e}): add yourself to the kvm group \
                 (`sudo usermod -aG kvm $USER`, then log out and back in)"
            ),
        },
    }
}

#[cfg(windows)]
fn probe_whp() -> AccelProbe {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let dll = std::path::Path::new(&root)
        .join("System32")
        .join("WinHvPlatform.dll");
    if dll.exists() {
        AccelProbe {
            accel: Accel::Whp,
            usable: true,
            detail: format!("{} found", dll.display()),
        }
    } else {
        AccelProbe {
            accel: Accel::Whp,
            usable: false,
            detail: "Windows Hypervisor Platform is not enabled. Run in an elevated PowerShell: \
                     `Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform` and reboot"
                .into(),
        }
    }
}

/// Error returned by the native VMM while it is under construction.
#[derive(Debug)]
pub struct NotYetImplemented;

impl fmt::Display for NotYetImplemented {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "the native bvmm backend is not implemented yet; use the qemu backend \
             (see docs/NATIVE-VMM.md for the roadmap)",
        )
    }
}

impl std::error::Error for NotYetImplemented {}

/// The future in-process VMM. Construction always fails for now.
pub struct NativeVmm {
    pub accel: Accel,
}

impl NativeVmm {
    pub fn new() -> Result<Self, NotYetImplemented> {
        Err(NotYetImplemented)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qemu_names() {
        assert_eq!(Accel::Kvm.qemu_name(), "kvm");
        assert_eq!(Accel::Whp.qemu_name(), "whpx");
        assert_eq!(Accel::None.qemu_name(), "tcg");
    }

    #[test]
    fn native_is_stub() {
        assert!(NativeVmm::new().is_err());
    }
}
