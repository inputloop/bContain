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
    /// Which controller the disk sits on.
    #[serde(default)]
    pub bus: DiskBus,
    /// A boot medium to install from (e.g. SteamOS's recovery image): it
    /// boots first until it is ejected.
    #[serde(default)]
    pub installer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskBus {
    /// virtio-blk, or IDE/SATA for legacy and compatibility VMs.
    #[default]
    Auto,
    /// An NVMe drive (SteamOS's installer writes to /dev/nvme0n1).
    Nvme,
    /// A USB stick on its own xHCI controller.
    Usb,
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
    /// What runs inside, where it changes the virtual hardware.
    #[serde(default)]
    pub guest: GuestOs,
    /// Open the VM's window full screen.
    #[serde(default)]
    pub fullscreen: bool,
    /// The resolution the virtual monitor reports as preferred (EDID).
    #[serde(default)]
    pub resolution: Option<(u32, u32)>,
    /// A TPM 2.0 from swtpm (Linux hosts). Windows 11 wants one.
    #[serde(default)]
    pub tpm: bool,
    /// UEFI Secure Boot with Microsoft's keys enrolled (Linux hosts: needs
    /// SMM, which WHPX lacks).
    #[serde(default)]
    pub secure_boot: bool,
    /// A host folder the guest can open over the network: QEMU's own SMB
    /// share on Linux hosts, a Windows share on Windows hosts.
    #[serde(default)]
    pub shared_folder: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GuestOs {
    #[default]
    Other,
    /// Windows 10 or 11: clock in local time, Hyper-V enlightenments on KVM.
    Windows,
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
    /// Press and release a key in the guest (a QEMU `qcode` such as `spc`).
    fn press_key(&self, _running: &Running, _key: &str) -> Result<()> {
        anyhow::bail!("{} cannot send keys", self.name())
    }
}

/// Pick the backend named by `name` ("qemu" or "native").
pub fn backend(name: &str) -> Result<Box<dyn Hypervisor>> {
    match name {
        "qemu" => Ok(Box::new(crate::qemu::Qemu::locate()?)),
        "native" => Ok(Box::new(crate::native::Native)),
        other => anyhow::bail!("unknown backend `{other}` (expected `qemu` or `native`)"),
    }
}
