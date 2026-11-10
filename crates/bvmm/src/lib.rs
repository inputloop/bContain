// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! bvmm: bContain's own hypervisor.
//!
//! It boots a Linux kernel straight into 64-bit mode on the host's hardware
//! virtualization: KVM on Linux ([`platform::kvm`]), Windows Hypervisor
//! Platform on Windows ([`platform::whp`]). The guest gets guest RAM, a
//! 16550 console, a CMOS clock, a reset line and an interrupt controller.
//! That is enough for bContain's container micro-VMs. Disk images and ISOs
//! still run on QEMU; see `docs/NATIVE-VMM.md` for the roadmap.
//!
//! * [`probe`] answers "which hardware accelerator can this host use?"
//! * [`self_test`] runs a few instructions of guest code to prove the
//!   platform layer works before a kernel is involved.
//! * [`run_linux`] boots a kernel and initramfs with the console on stdio.

use std::fmt;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

pub mod boot;
pub mod devices;
pub mod layout;
pub mod memory;
pub mod mptable;
pub mod platform;
pub mod virtio;

use devices::Devices;
use memory::GuestMemory;

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

/// Errors from the native VMM, already phrased for the person running it.
#[derive(Debug)]
pub struct Error(String);

impl Error {
    pub fn new(msg: impl Into<String>) -> Error {
        Error(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// How a guest run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The guest reset the machine. Container init reboots to exit, so this
    /// is the normal way a container finishes.
    Reset,
    /// The vCPU halted with interrupts off: nothing more can happen.
    Halted,
}

/// Kernel parameters the native platform needs on top of the caller's:
/// legacy-device probes that would only waste boot time, reboot through the
/// keyboard controller (how our init exits), no ACPI (none emulated), and no
/// IO-APIC timer check (the timer is the local APIC's).
pub const KERNEL_PARAMS: &str = "reboot=k panic=-1 acpi=off no_timer_check tsc=reliable \
     i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd";
/// Without PCI devices the kernel is told not to look for a bus at all.
const NO_PCI: &str = "pci=off";
/// With them, it is told how to reach configuration space: there is no
/// firmware to say so.
const WITH_PCI: &str = "pci=conf1";

/// A disk image presented to the guest as a virtio block device.
#[derive(Debug, Clone)]
pub struct DiskConfig {
    pub path: std::path::PathBuf,
    pub read_only: bool,
}

#[derive(Default)]
pub struct VmConfig {
    pub memory_mib: u64,
    pub kernel: Vec<u8>,
    pub initrd: Option<Vec<u8>>,
    pub cmdline: String,
    /// Raw disk images, in order: the first is `/dev/vda` in a Linux guest.
    pub disks: Vec<DiskConfig>,
}

/// Boot `cfg` with the guest console on `console` and, if given, keyboard
/// input from `input`. Returns when the guest resets or halts.
pub fn run_linux(
    cfg: &VmConfig,
    console: Box<dyn Write + Send>,
    input: Option<Box<dyn Read + Send>>,
) -> Result<Outcome> {
    let probe = probe();
    if !probe.usable {
        return Err(Error::new(format!(
            "{} is not available: {}",
            probe.accel, probe.detail
        )));
    }
    let mem = Arc::new(GuestMemory::new(cfg.memory_mib << 20)?);
    const CPUS: u8 = 1;
    mptable::write(&mem, CPUS)?;
    let mut bus = devices::pci::PciBus::new(mem.clone());
    for disk in &cfg.disks {
        let blk = virtio::blk::Block::open(&disk.path, disk.read_only)?;
        bus.add_virtio(Box::new(blk));
    }
    let pci = if bus.is_empty() { NO_PCI } else { WITH_PCI };
    let cmdline = format!("{} {} {}", cfg.cmdline.trim(), KERNEL_PARAMS, pci);
    let boot = boot::load_linux(&mem, &cfg.kernel, cfg.initrd.as_deref(), &cmdline)?;
    let mut devices = Devices::new(console, platform::NEEDS_IOAPIC, CPUS);
    if !bus.is_empty() {
        devices = devices.with_pci(bus);
    }
    let devices = Arc::new(devices);
    if let Some(mut input) = input {
        let d = devices.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            while let Ok(n) = input.read(&mut buf) {
                if n == 0 {
                    break;
                }
                d.console_input(&buf[..n]);
            }
        });
    }
    let outcome = platform::run(&mem, &boot, devices.clone());
    devices.flush();
    outcome
}

/// Guest code for [`self_test`], 64-bit:
/// prints "OK" on COM1, reads the I/O APIC version register over MMIO and
/// prints its low byte, prints "\n", then resets via the keyboard controller.
const SELF_TEST_CODE: &[u8] = &[
    0x66, 0xba, 0xf8, 0x03, // mov dx, 0x3f8
    0xb0, b'O', 0xee, // mov al, 'O' ; out dx, al
    0xb0, b'K', 0xee, // mov al, 'K' ; out dx, al
    0x48, 0xbb, 0x00, 0x00, 0xc0, 0xfe, 0x00, 0x00, 0x00, 0x00, // mov rbx, 0xfec00000
    0xc7, 0x03, 0x01, 0x00, 0x00, 0x00, // mov dword [rbx], 1   (IOREGSEL = version)
    0x8b, 0x43, 0x10, // mov eax, [rbx+0x10]                    (IOWIN)
    0xee, // out dx, al
    0xb0, b'\n', 0xee, // mov al, 10 ; out dx, al
    0xb0, 0xfe, 0xe6, 0x64, // mov al, 0xfe ; out 0x64, al      (reset)
    0xf4, // hlt
    0xeb, 0xfd, // jmp back to hlt
];

/// Expected console output of the self test.
pub const SELF_TEST_EXPECTED: &[u8] = b"OK\x11\n";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Run a few instructions of real guest code on this host's hypervisor and
/// check what they printed. Proves memory mapping, long-mode entry, port I/O
/// exits, MMIO exits (to the I/O APIC) and reset handling, step by step.
/// `step` is told what is happening, so a failure says where it stopped.
pub fn self_test(step: &mut dyn FnMut(&str)) -> Result<()> {
    let probe = probe();
    step(&format!("accelerator: {} ({})", probe.accel, probe.detail));
    if !probe.usable {
        return Err(Error::new("no usable hardware accelerator"));
    }
    let mem = GuestMemory::new(64 << 20)?;
    let boot = boot::setup_long_mode(&mem, layout::SELFTEST_CODE_ADDR)?;
    mem.write(layout::SELFTEST_CODE_ADDR, SELF_TEST_CODE)?;
    step("guest memory, page tables and GDT ready");
    let capture = Capture::default();
    let devices = Arc::new(Devices::new(
        Box::new(capture.clone()),
        platform::NEEDS_IOAPIC,
        1,
    ));

    // A guest that never resets would hang the test: run it on a thread and give up after 10 s.
    let (tx, rx) = std::sync::mpsc::channel();
    let d = devices.clone();
    std::thread::spawn(move || {
        let r = platform::run(&mem, &boot, d);
        let _ = tx.send(r);
    });
    let outcome = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .map_err(|_| Error::new("the guest did not finish within 10 seconds"))??;
    let out = capture.0.lock().unwrap().clone();
    step(&format!(
        "guest finished ({outcome:?}); console printed {out:02x?}"
    ));
    if out != SELF_TEST_EXPECTED {
        return Err(Error::new(format!(
            "unexpected guest output {out:02x?}, wanted {SELF_TEST_EXPECTED:02x?}"
        )));
    }
    if outcome != Outcome::Reset {
        return Err(Error::new("the guest did not reset at the end"));
    }
    step("port I/O, MMIO (I/O APIC version 0x11) and reset all work");
    Ok(())
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
    fn self_test_code_is_what_the_comments_say() {
        // 40 bytes; the final jmp lands on the hlt before it.
        assert_eq!(SELF_TEST_CODE.len(), 40);
        assert_eq!(SELF_TEST_CODE[37], 0xf4);
        assert_eq!(&SELF_TEST_CODE[38..], &[0xeb, 0xfd]);
    }
}
