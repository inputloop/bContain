// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Backend-neutral VM description and the `Hypervisor` trait.
//!
//! Everything above this module (CLI, web UI, OCI runner) builds a [`VmSpec`]
//! and hands it to a [`Hypervisor`]. The QEMU backend implements it today; the
//! native `bvmm` backend will implement the same trait later, so swapping
//! engines does not touch the rest of bContain.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::image::DiskFormat;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Disk {
    pub path: PathBuf,
    pub format: DiskFormat,
    /// Attach as a CD-ROM (ISO) instead of a writable disk.
    #[serde(default)]
    pub cdrom: bool,
    /// Discard writes on shutdown (QEMU `snapshot=on`).
    #[serde(default)]
    pub ephemeral: bool,
}

/// Direct kernel boot, used for container micro-VMs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelBoot {
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
    pub cmdline: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Display {
    /// virtio-gpu with virgl 3D acceleration on the host GPU.
    Virgl,
    /// Standard VGA without 3D: works with every guest OS.
    Basic,
    /// No window; serial console on stdio.
    Headless,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Firmware {
    Bios,
    Uefi,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortForward {
    pub host: u16,
    pub guest: u16,
    /// UDP instead of TCP.
    #[serde(default)]
    pub udp: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmSpec {
    pub name: String,
    pub cpus: u32,
    pub memory_mib: u64,
    pub disks: Vec<Disk>,
    pub kernel: Option<KernelBoot>,
    pub display: Display,
    pub firmware: Firmware,
    #[serde(default)]
    pub ports: Vec<PortForward>,
    /// Back guest RAM with hugepages when the host has enough reserved (Linux).
    #[serde(default)]
    pub hugepages: bool,
    /// Exit instead of rebooting when the guest reboots or its init exits.
    #[serde(default)]
    pub no_reboot: bool,
    /// Emulate SATA disks and an e1000e NIC instead of virtio, for guests
    /// without virtio drivers (stock Windows installers).
    #[serde(default)]
    pub legacy_devices: bool,
    /// How to use the hardware hypervisor; see [`AccelMode`].
    #[serde(default)]
    pub accel_mode: AccelMode,
    /// A host GPU handed to the VM with VFIO (Linux + KVM only).
    #[serde(default)]
    pub gpu: Option<GpuPassthrough>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuPassthrough {
    /// PCI functions, GPU first (e.g. 0000:01:00.0 and its audio 0000:01:00.1).
    pub functions: Vec<String>,
    /// The host card's subsystem IDs, so the guest driver sees the laptop's
    /// own GPU rather than an anonymous one.
    pub subsystem_vendor: String,
    pub subsystem_device: String,
    /// Video BIOS image for GPUs whose ROM is not on the card (laptops).
    #[serde(default)]
    pub rom: Option<PathBuf>,
}

/// WHPX in particular behaves differently across Windows builds and guests,
/// so the hardware-acceleration mode is a per-VM choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccelMode {
    /// KVM / WHPX with QEMU's defaults (WHPX uses Hyper-V's own APIC when available).
    #[default]
    Auto,
    /// WHPX with QEMU's userspace APIC (`kernel-irqchip=off`) on classic PC
    /// hardware (i440FX, IDE, e1000, UHCI: no MSI), and a CPU without XSAVE or
    /// AVX (QEMU 11's WHPX XSAVE sync fails on some Windows builds). For Windows builds where
    /// the default hangs before the BIOS shows anything, and for small or old
    /// guests.
    Compat,
    /// No hardware acceleration (QEMU TCG): slow, but independent of the hypervisor.
    Software,
}

/// A running VM as seen by its backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Running {
    pub pid: u32,
    /// Local TCP port of the control channel (QMP for QEMU).
    pub control_port: u16,
}

pub trait Hypervisor {
    fn name(&self) -> &'static str;
    /// Explain why this backend cannot run on this host, if it cannot.
    fn check(&self) -> Result<()>;
    /// Start a VM in the background and return its handle.
    fn launch(&self, spec: &VmSpec, log: &std::path::Path) -> Result<Running>;
    /// Start a VM attached to this terminal (serial console on stdio) and wait for it.
    fn run_foreground(&self, spec: &VmSpec) -> Result<i32>;
    /// Ask the guest to power off (ACPI), or force it off.
    fn stop(&self, running: &Running, force: bool) -> Result<()>;
    fn is_alive(&self, running: &Running) -> bool;
}

/// Pick the backend named by `name` ("qemu" or "native").
pub fn backend(name: &str) -> Result<Box<dyn Hypervisor>> {
    match name {
        "qemu" => Ok(Box::new(crate::qemu::Qemu::locate()?)),
        "native" => Ok(Box::new(crate::native::Native)),
        other => anyhow::bail!("unknown backend `{other}` (expected `qemu` or `native`)"),
    }
}
