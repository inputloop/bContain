// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! QEMU backend: hardware-accelerated with WHPX on Windows and KVM on Linux.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bvmm::Accel;

use crate::engine::{AccelMode, DiskBus, Display, Firmware, GuestOs, Hypervisor, Running, VmSpec};

const EXE: &str = if cfg!(windows) {
    "qemu-system-x86_64.exe"
} else {
    "qemu-system-x86_64"
};

pub struct Qemu {
    pub binary: PathBuf,
    pub accel: Accel,
}

impl Qemu {
    /// Find QEMU: `$BCONTAIN_QEMU`, then a `qemu/` folder bundled next to the
    /// bcontain executable (the standalone layout), then the usual install
    /// locations and `PATH`.
    pub fn locate() -> Result<Qemu> {
        let binary = find_binary().with_context(|| {
            format!(
                "{EXE} not found. Put a QEMU build in a `qemu` folder next to bcontain \
                 (scripts/bundle-qemu-windows.ps1 does this on Windows), install QEMU \
                 (`sudo apt install qemu-system-x86` / `sudo dnf install qemu-kvm`), or set \
                 BCONTAIN_QEMU to the full path"
            )
        })?;
        let probe = bvmm::probe();
        let accel = if probe.usable {
            probe.accel
        } else {
            Accel::None
        };
        Ok(Qemu { binary, accel })
    }

    pub fn version(&self) -> Option<String> {
        let out = crate::desktop::helper(Command::new(&self.binary).arg("--version"))
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .map(str::to_string)
    }

    /// The firmware directory QEMU ships with (`share/` on Windows builds).
    fn share_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(bin_dir) = self.binary.parent() {
            dirs.push(bin_dir.join("share"));
            dirs.push(bin_dir.join("..").join("share").join("qemu"));
        }
        dirs.extend(
            [
                "/usr/share/OVMF",
                "/usr/share/ovmf",
                "/usr/share/edk2/ovmf",
                "/usr/share/edk2/x64",
                "/usr/share/qemu",
            ]
            .map(PathBuf::from),
        );
        dirs
    }

    fn find_uefi_code(&self) -> Option<PathBuf> {
        const NAMES: [&str; 5] = [
            "OVMF_CODE_4M.fd",
            "OVMF_CODE.fd",
            "edk2-x86_64-code.fd",
            "OVMF_CODE.secboot.fd",
            "OVMF.fd",
        ];
        // Prefer by name over by folder: a split code + variables pair beats
        // a combined OVMF.fd, which cannot keep per-VM boot entries.
        let dirs = self.share_dirs();
        NAMES
            .iter()
            .flat_map(|n| dirs.iter().map(move |d| d.join(n)))
            .find(|p| p.is_file())
    }

    /// Build the full QEMU argument list for `spec`.
    pub fn args(&self, spec: &VmSpec, foreground: bool) -> Result<Vec<String>> {
        let mut a: Vec<String> = Vec::new();
        let mut push = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));

        push(&["-name", &spec.name]);
        // Compatibility mode also means compatible hardware: the i440FX PC
        // with IDE, e1000 and UHCI. Every device then interrupts on a plain
        // IRQ line and is driven through port I/O: no MSI/MSI-X, little MMIO.
        // That suits QEMU's userspace APIC on WHPX and the oldest, smallest
        // guests (a Tiny Core boot stalled reading its CD over AHCI).
        let pc_hw = spec.accel_mode == AccelMode::Compat;
        let chipset = if pc_hw { "pc" } else { "q35" };
        let mut machine = chipset.to_string();
        if spec.hugepages {
            machine.push_str(",memory-backend=ram0");
        }
        // No HPET for Windows, as libvirt's and virt-manager's Windows
        // profiles do: Windows otherwise drives its clock from QEMU's
        // emulated HPET, which some builds hang on early in boot (the
        // firmware's or Windows' logo, never the spinner).
        if spec.guest == GuestOs::Windows {
            machine.push_str(",hpet=off");
        }
        push(&["-machine", &machine]);
        let accel = if spec.accel_mode == AccelMode::Software {
            Accel::None
        } else {
            self.accel
        };
        match accel {
            // Auto leaves kernel-irqchip to QEMU, which uses Hyper-V's own APIC
            // when Windows offers it. Its userspace APIC (Compat) aborts some
            // guests (Tiny Core), while the in-kernel one hangs some Windows
            // builds before the BIOS runs. Hence the per-VM choice.
            Accel::Whp if pc_hw => push(&["-accel", "whpx,kernel-irqchip=off"]),
            Accel::Whp => push(&["-accel", "whpx"]),
            Accel::Kvm => push(&["-accel", "kvm"]),
            Accel::None => push(&["-accel", "tcg,thread=multi"]),
        }
        // Compatibility mode hides XSAVE, and the AVX family that needs it,
        // from the guest. QEMU 11's WHPX code syncs the XSAVE area whenever
        // the guest has enabled it, and some Windows builds return it empty,
        // which kills QEMU ("failed to get xsave state: No error"). Old and
        // small guests do not need AVX.
        let windows = spec.guest == GuestOs::Windows;
        let cpu = match (accel, pc_hw) {
            (_, true) => "max,-xsave,-avx,-avx2,-fma,-f16c,-avx-vnni,-vaes,-vpclmulqdq",
            // Hyper-V enlightenments: Windows then uses KVM's paravirtual
            // clock, timers and spinlocks instead of polling emulated hardware.
            (Accel::Kvm, false) if windows => {
                "host,hv_relaxed,hv_vapic,hv_spinlocks=0x1fff,hv_vpindex,hv_runtime,hv_time,hv_synic,hv_stimer,hv_frequencies"
            }
            (Accel::Kvm, false) => "host",
            _ => "max",
        };
        push(&["-cpu", cpu]);
        if windows {
            // Windows keeps the hardware clock in local time.
            push(&["-rtc", "base=localtime"]);
        }
        let smp = format!("{0},sockets=1,cores={0},threads=1", spec.cpus);
        push(&["-smp", &smp]);
        let mem = format!("{}M", spec.memory_mib);
        push(&["-m", &mem]);
        if spec.hugepages {
            let backend = format!(
                "memory-backend-file,id=ram0,size={mem},mem-path=/dev/hugepages,prealloc=on,share=on"
            );
            push(&["-object", &backend]);
        }

        if spec.firmware == Firmware::Uefi {
            let code = self.find_uefi_code().context(
                "UEFI firmware (OVMF) not found; install `ovmf` / `edk2-ovmf` or use --firmware bios",
            )?;
            let drive = format!("if=pflash,format=raw,readonly=on,file={}", qemu_path(&code));
            push(&["-drive", &drive]);
            // Per-VM variable store, so an installed OS keeps its boot entry.
            if let Some(vars) = uefi_vars_for(&code, &spec.name)? {
                let drive = format!("if=pflash,format=raw,file={}", qemu_path(&vars));
                push(&["-drive", &drive]);
            }
        }

        if let Some(gpu) = &spec.gpu {
            if accel != Accel::Kvm {
                bail!("GPU passthrough needs KVM on a Linux host");
            }
            // Behind its own PCIe root port, as on real hardware: the NVIDIA
            // driver reads link details from the port.
            push(&["-device", "pcie-root-port,id=gpuport,chassis=1,slot=1"]);
            for (i, f) in gpu.functions.iter().enumerate() {
                let mut dev = format!("vfio-pci,host={f},bus=gpuport,addr=00.{i}");
                if i == 0 {
                    dev.push_str(&format!(
                        ",multifunction=on,x-pci-sub-vendor-id={},x-pci-sub-device-id={}",
                        gpu.subsystem_vendor, gpu.subsystem_device
                    ));
                    if let Some(rom) = &gpu.rom {
                        dev.push_str(&format!(",romfile={}", qemu_path(rom)));
                    }
                }
                push(&["-device", &dev]);
            }
        }

        let mut has_cdrom = false;
        // With an installer attached, boot order is explicit: installer
        // first, then the other disks. (`bootindex` overrides `-boot`.)
        let has_installer = spec.disks.iter().any(|d| d.installer);
        let mut next_boot = 1;
        let mut usb_controller = false;
        // Place every IDE/SATA drive explicitly: q35's AHCI controller has
        // six ports with one drive each; the i440FX has two IDE buses with
        // two units each (QEMU's own placement fails at the third drive).
        let mut ide_slot = 0;
        let mut ide_bus = |dev: &mut String| {
            if pc_hw {
                dev.push_str(&format!(",bus=ide.{},unit={}", ide_slot / 2, ide_slot % 2));
            } else {
                dev.push_str(&format!(",bus=ide.{ide_slot}"));
            }
            ide_slot += 1;
        };
        for (i, d) in spec.disks.iter().enumerate() {
            if d.cdrom {
                has_cdrom = true;
                let drive = format!(
                    "file={},format=raw,media=cdrom,readonly=on,if=none,id=cd{i}",
                    qemu_path(&d.path)
                );
                let mut dev = format!("ide-cd,drive=cd{i}");
                ide_bus(&mut dev);
                if d.installer {
                    dev.push_str(",bootindex=0");
                }
                push(&["-drive", &drive, "-device", &dev]);
                continue;
            }
            let mut drive = format!(
                "file={},format={},if=none,id=disk{i},discard=unmap",
                qemu_path(&d.path),
                d.format.qemu_name()
            );
            if d.ephemeral {
                drive.push_str(",snapshot=on");
            }
            push(&["-drive", &drive]);
            let mut dev = match d.bus {
                DiskBus::Nvme => format!("nvme,drive=disk{i},serial=bcontain{i}"),
                DiskBus::Usb => {
                    if !usb_controller {
                        push(&["-device", "qemu-xhci,id=usbdisks"]);
                        usb_controller = true;
                    }
                    format!("usb-storage,bus=usbdisks.0,drive=disk{i},removable=on")
                }
                DiskBus::Auto if spec.legacy_devices || pc_hw => {
                    let mut dev = format!("ide-hd,drive=disk{i}");
                    ide_bus(&mut dev);
                    dev
                }
                DiskBus::Auto => format!("virtio-blk-pci,drive=disk{i}"),
            };
            if has_installer {
                let index = if d.installer { 0 } else { next_boot };
                if !d.installer {
                    next_boot += 1;
                }
                dev.push_str(&format!(",bootindex={index}"));
            }
            push(&["-device", &dev]);
        }
        if has_cdrom {
            // Boot the ISO once (installers), then the disk on later boots.
            push(&["-boot", "once=d,menu=on"]);
        }

        if let Some(k) = &spec.kernel {
            push(&["-kernel", &k.kernel.to_string_lossy()]);
            if let Some(initrd) = &k.initrd {
                push(&["-initrd", &initrd.to_string_lossy()]);
            }
            push(&["-append", &k.cmdline]);
        }

        let mut netdev = "user,id=net0".to_string();
        for p in &spec.ports {
            let proto = if p.udp { "udp" } else { "tcp" };
            netdev.push_str(&format!(
                ",hostfwd={proto}:127.0.0.1:{}-:{}",
                p.host, p.guest
            ));
        }
        push(&["-netdev", &netdev]);
        let nic = if pc_hw {
            "e1000,netdev=net0"
        } else if spec.legacy_devices {
            "e1000e,netdev=net0"
        } else {
            "virtio-net-pci,netdev=net0"
        };
        push(&["-device", nic]);
        if !pc_hw {
            push(&["-device", "virtio-rng-pci"]);
        }

        let gui_backend = if cfg!(windows) { "sdl" } else { "gtk" };
        match spec.display {
            Display::Virgl => {
                let disp = format!("{gui_backend},gl=on");
                push(&["-device", "virtio-vga-gl", "-display", &disp]);
            }
            // Standard VGA: every OS drives it (VGA text, VBE, Linux bochs-drm,
            // Windows' basic display). virtio-vga hands its output over to a
            // virtio-gpu driver, and guests that load it but never set a mode
            // (e.g. Tiny Core) leave QEMU showing "Display output is not active".
            Display::Basic => push(&["-vga", "std", "-display", gui_backend]),
            Display::Headless => {
                push(&["-display", "none"]);
                if foreground {
                    push(&["-serial", "mon:stdio"]);
                } else {
                    // stdout is the VM's log file when launched in the background.
                    push(&["-serial", "stdio"]);
                }
            }
        }
        if spec.display != Display::Headless {
            if pc_hw {
                push(&["-usb", "-device", "usb-tablet"]); // PIIX3 UHCI
            } else {
                push(&["-device", "qemu-xhci", "-device", "usb-tablet"]);
            }
        }
        if spec.no_reboot {
            push(&["-no-reboot"]);
        }
        Ok(a)
    }

    /// [`Qemu::command`] for a VM that runs on its own: on Windows, without
    /// a console window and outside the app's Ctrl+C group.
    pub fn background_command(&self, args: &[String]) -> Command {
        #[allow(unused_mut)]
        let mut cmd = self.command(args);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        }
        cmd
    }

    pub fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.args(args);
        if let Some(dir) = self.binary.parent() {
            // Windows QEMU builds look for their DLLs and firmware beside the exe.
            cmd.current_dir(dir);
        }
        cmd
    }
}

impl Hypervisor for Qemu {
    fn name(&self) -> &'static str {
        "qemu"
    }

    fn check(&self) -> Result<()> {
        if !self.binary.is_file() {
            bail!("{} does not exist", self.binary.display());
        }
        Ok(())
    }

    fn launch(&self, spec: &VmSpec, log: &Path) -> Result<Running> {
        let port = free_port()?;
        let mut args = self.args(spec, false)?;
        args.extend([
            "-qmp".into(),
            format!("tcp:127.0.0.1:{port},server=on,wait=off"),
        ]);
        let out = File::create(log).with_context(|| format!("creating {}", log.display()))?;
        writeln!(&out, "# {} {}", self.binary.display(), shell_join(&args))?;
        let mut cmd = self.background_command(&args);
        cmd.stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting {}", self.binary.display()))?;

        // Catch immediate failures (bad args, missing accel) instead of
        // reporting a VM that is already dead.
        for _ in 0..20 {
            if let Some(status) = child.try_wait()? {
                bail!("qemu exited at startup ({status}):\n{}", tail(log, 15));
            }
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        Ok(Running {
            pid: child.id(),
            control_port: port,
        })
    }

    fn run_foreground(&self, spec: &VmSpec) -> Result<i32> {
        let args = self.args(spec, true)?;
        let status = self
            .command(&args)
            .status()
            .with_context(|| format!("starting {}", self.binary.display()))?;
        Ok(status.code().unwrap_or(-1))
    }

    fn stop(&self, running: &Running, force: bool) -> Result<()> {
        qmp(
            running.control_port,
            if force { "quit" } else { "system_powerdown" },
        )?;
        if force {
            // Letting go of a passed-through GPU can take tens of seconds.
            for _ in 0..600 {
                if !self.is_alive(running) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            bail!("qemu (pid {}) did not exit after `quit`", running.pid);
        }
        Ok(())
    }

    fn press_key(&self, running: &Running, key: &str) -> Result<()> {
        let args = serde_json::json!({ "keys": [{ "type": "qcode", "data": key }] });
        qmp_with(running.control_port, "send-key", Some(args))
    }

    fn is_alive(&self, running: &Running) -> bool {
        let mut sys = sysinfo::System::new();
        let pid = sysinfo::Pid::from_u32(running.pid);
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        sys.process(pid).is_some_and(|p| {
            !matches!(
                p.status(),
                sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
            ) && p.name().to_string_lossy().to_lowercase().contains("qemu")
        })
    }
}

/// Send one QMP command and wait for its reply.
fn qmp(port: u16, command: &str) -> Result<()> {
    qmp_with(port, command, None)
}

/// Send one QMP command, with arguments, and wait for its reply.
pub fn qmp_with(port: u16, command: &str, arguments: Option<serde_json::Value>) -> Result<()> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .with_context(|| format!("connecting to QMP on port {port}; is the VM running?"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut w = stream;
    let mut line = String::new();
    reader.read_line(&mut line)?; // greeting
    for cmd in ["qmp_capabilities", command] {
        let mut msg = serde_json::json!({ "execute": cmd });
        if cmd == command {
            if let Some(a) = &arguments {
                msg["arguments"] = a.clone();
            }
        }
        writeln!(w, "{msg}")?;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return Ok(()), // `quit` closes the socket
                Ok(_) => {}
                Err(e)
                    if cmd == "quit"
                        && matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) =>
                {
                    return Ok(())
                }
                Err(e) => return Err(e.into()),
            }
            let v: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
            if let Some(err) = v.get("error") {
                bail!("QMP {cmd} failed: {err}");
            }
            if v.get("return").is_some() {
                break;
            }
        }
    }
    Ok(())
}

fn find_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("BCONTAIN_QEMU") {
        return Some(PathBuf::from(p));
    }
    let mut candidates = Vec::new();
    match embedded::unpack() {
        Ok(Some(p)) => return Some(p),
        Ok(None) => {}
        Err(e) => eprintln!("warning: could not unpack the bundled QEMU: {e:#}"),
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("qemu").join(EXE));
            candidates.push(dir.join(EXE));
        }
    }
    if cfg!(windows) {
        candidates.push(PathBuf::from(r"C:\Program Files\qemu").join(EXE));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|d| d.join(EXE)));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// The QEMU build carried inside the executable (single-file Windows build).
mod embedded {
    use std::path::PathBuf;

    use anyhow::Result;

    #[cfg(embedded_qemu)]
    static PAYLOAD: &[u8] = include_bytes!(env!("BCONTAIN_QEMU_PAYLOAD"));

    /// Unpack once per payload into `<data>/qemu/embedded-<size>/` and return
    /// the QEMU binary there. `None` when this build carries no QEMU.
    pub fn unpack() -> Result<Option<PathBuf>> {
        #[cfg(not(embedded_qemu))]
        {
            Ok(None)
        }
        #[cfg(embedded_qemu)]
        {
            use anyhow::Context;
            let dir = crate::store::data_dir()
                .join("qemu")
                .join(concat!("embedded-", env!("BCONTAIN_QEMU_PAYLOAD_LEN")));
            let exe = dir.join(super::EXE);
            if dir.join(".complete").is_file() && exe.is_file() {
                return Ok(Some(exe));
            }
            eprintln!(
                "unpacking bundled QEMU to {} (first run only)",
                dir.display()
            );
            let tmp = dir.with_extension("partial");
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp)?;
            tar::Archive::new(flate2::read::GzDecoder::new(PAYLOAD))
                .unpack(&tmp)
                .context("unpacking QEMU payload")?;
            std::fs::write(tmp.join(".complete"), b"")?;
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::rename(&tmp, &dir)?;
            Ok(Some(exe).filter(|p| p.is_file()))
        }
    }

    /// Whether this executable carries its own QEMU.
    pub const PRESENT: bool = cfg!(embedded_qemu);
}

pub fn has_embedded_qemu() -> bool {
    embedded::PRESENT
}

/// The writable UEFI variable store for VM `name`, copied on first use from
/// the template that matches `code`. `None` for combined images (OVMF.fd).
fn uefi_vars_for(code: &Path, name: &str) -> Result<Option<PathBuf>> {
    let file = code
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let template = match file {
        "OVMF_CODE_4M.fd" => "OVMF_VARS_4M.fd",
        "OVMF_CODE.fd" | "OVMF_CODE.secboot.fd" => "OVMF_VARS.fd",
        "edk2-x86_64-code.fd" => "edk2-i386-vars.fd",
        _ => return Ok(None),
    };
    let template = code.with_file_name(template);
    if !template.is_file() {
        return Ok(None);
    }
    let vars = crate::store::vm_dir(name).join("efivars.fd");
    if !vars.is_file() {
        fs::create_dir_all(crate::store::vm_dir(name))?;
        fs::copy(&template, &vars).with_context(|| format!("copying {}", template.display()))?;
    }
    Ok(Some(vars))
}

/// QEMU option values use `,` as a separator; a literal comma is written `,,`.
fn qemu_path(p: &Path) -> String {
    p.to_string_lossy().replace(',', ",,")
}

pub fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

pub fn tail(path: &Path, lines: usize) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

pub fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.chars().any(|c| c.is_whitespace() || c == '"') {
                format!("\"{}\"", a.replace('"', "\\\""))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Disk, PortForward};
    use crate::image::DiskFormat;

    fn spec() -> VmSpec {
        VmSpec {
            name: "t".into(),
            cpus: 10,
            memory_mib: 16384,
            disks: vec![
                Disk {
                    path: "/vm/a,b.qcow2".into(),
                    format: DiskFormat::Qcow2,
                    cdrom: false,
                    ephemeral: false,
                    bus: DiskBus::Auto,
                    installer: false,
                },
                Disk {
                    path: "/iso/x.iso".into(),
                    format: DiskFormat::Iso,
                    cdrom: true,
                    ephemeral: false,
                    bus: DiskBus::Auto,
                    installer: false,
                },
            ],
            kernel: None,
            display: Display::Virgl,
            firmware: Firmware::Bios,
            ports: vec![
                PortForward {
                    host: 8080,
                    guest: 80,
                    udp: false,
                },
                PortForward {
                    host: 47998,
                    guest: 47998,
                    udp: true,
                },
            ],
            hugepages: false,
            no_reboot: false,
            legacy_devices: false,
            accel_mode: AccelMode::Auto,
            gpu: None,
            guest: GuestOs::Other,
        }
    }

    #[test]
    fn builds_args() {
        let q = Qemu {
            binary: "/usr/bin/qemu-system-x86_64".into(),
            accel: Accel::Kvm,
        };
        let a = q.args(&spec(), false).unwrap().join(" ");
        assert!(a.contains("-accel kvm -cpu host"));
        assert!(a.contains("-smp 10,sockets=1,cores=10,threads=1"));
        assert!(a.contains("-m 16384M"));
        assert!(a.contains("file=/vm/a,,b.qcow2,format=qcow2"));
        assert!(a.contains("-device virtio-blk-pci,drive=disk0"));
        assert!(a.contains("-boot once=d"));
        assert!(a.contains("hostfwd=tcp:127.0.0.1:8080-:80,hostfwd=udp:127.0.0.1:47998-:47998"));
        assert!(a.contains("virtio-vga-gl"));
    }

    #[test]
    fn steamos_disks() {
        let q = Qemu {
            binary: "qemu".into(),
            accel: Accel::Kvm,
        };
        let mut s = spec();
        s.disks = vec![
            Disk {
                path: "/vm/nvme.qcow2".into(),
                format: DiskFormat::Qcow2,
                cdrom: false,
                ephemeral: false,
                bus: DiskBus::Nvme,
                installer: false,
            },
            Disk {
                path: "/img/steamdeck-repair.img".into(),
                format: DiskFormat::Raw,
                cdrom: false,
                ephemeral: true,
                bus: DiskBus::Usb,
                installer: true,
            },
        ];
        let a = q.args(&s, false).unwrap().join(" ");
        assert!(a.contains("-device nvme,drive=disk0,serial=bcontain0,bootindex=1"));
        assert!(a.contains("-device qemu-xhci,id=usbdisks"));
        assert!(a.contains("usb-storage,bus=usbdisks.0,drive=disk1,removable=on,bootindex=0"));
        assert!(a.contains(
            "steamdeck-repair.img,format=raw,if=none,id=disk1,discard=unmap,snapshot=on"
        ));
        assert!(!a.contains("-boot once"));
        // Without an installer there is no explicit boot order.
        s.disks.pop();
        assert!(!q.args(&s, false).unwrap().join(" ").contains("bootindex"));
    }

    #[test]
    fn windows_guest() {
        let q = Qemu {
            binary: "qemu".into(),
            accel: Accel::Kvm,
        };
        let mut s = spec();
        s.guest = GuestOs::Windows;
        s.legacy_devices = true;
        let cd = |path: &str, installer| Disk {
            path: path.into(),
            format: DiskFormat::Iso,
            cdrom: true,
            ephemeral: false,
            bus: DiskBus::Auto,
            installer,
        };
        s.disks = vec![
            Disk {
                path: "/vm/windows.qcow2".into(),
                format: DiskFormat::Qcow2,
                cdrom: false,
                ephemeral: false,
                bus: DiskBus::Nvme,
                installer: false,
            },
            cd("/img/Win11.iso", true),
            cd("/vm/autounattend.iso", false),
        ];
        let a = q.args(&s, false).unwrap().join(" ");
        assert!(a.contains("-cpu host,hv_relaxed,"));
        assert!(a.contains("-rtc base=localtime"));
        assert!(a.contains("-machine q35,hpet=off"));
        assert!(a.contains("nvme,drive=disk0,serial=bcontain0,bootindex=1"));
        // One drive per AHCI port, the installer first in the boot order.
        assert!(a.contains("ide-cd,drive=cd1,bus=ide.0,bootindex=0"));
        assert!(a.contains("ide-cd,drive=cd2,bus=ide.1 "));
        assert!(a.contains("e1000e,netdev=net0"));
        // Classic PC hardware: two IDE buses, two units each.
        s.accel_mode = AccelMode::Compat;
        s.disks[0].bus = DiskBus::Auto;
        let a = q.args(&s, false).unwrap().join(" ");
        assert!(a.contains("ide-hd,drive=disk0,bus=ide.0,unit=0"));
        assert!(a.contains("ide-cd,drive=cd1,bus=ide.0,unit=1,bootindex=0"));
        assert!(a.contains("ide-cd,drive=cd2,bus=ide.1,unit=0 "));
    }

    #[test]
    fn gpu_passthrough() {
        let q = Qemu {
            binary: "/usr/bin/qemu-system-x86_64".into(),
            accel: Accel::Kvm,
        };
        let mut s = spec();
        s.gpu = Some(crate::engine::GpuPassthrough {
            functions: vec!["0000:01:00.0".into(), "0000:01:00.1".into()],
            subsystem_vendor: "0x1043".into(),
            subsystem_device: "0x16c3".into(),
            rom: Some("/vm/vbios.rom".into()),
        });
        let a = q.args(&s, false).unwrap().join(" ");
        assert!(a.contains("-device pcie-root-port,id=gpuport,chassis=1,slot=1"));
        assert!(a.contains("vfio-pci,host=0000:01:00.0,bus=gpuport,addr=00.0,multifunction=on,x-pci-sub-vendor-id=0x1043,x-pci-sub-device-id=0x16c3,romfile=/vm/vbios.rom"));
        assert!(a.contains("vfio-pci,host=0000:01:00.1,bus=gpuport,addr=00.1"));
        let w = Qemu {
            binary: "qemu".into(),
            accel: Accel::Whp,
        };
        assert!(w.args(&s, false).unwrap_err().to_string().contains("KVM"));
    }

    #[test]
    fn whpx_and_legacy() {
        let q = Qemu {
            binary: "qemu".into(),
            accel: Accel::Whp,
        };
        let mut s = spec();
        s.legacy_devices = true;
        s.display = Display::Headless;
        let a = q.args(&s, true).unwrap().join(" ");
        assert!(a.contains("-accel whpx -cpu max"));
        assert!(!a.contains("kernel-irqchip"));
        let mut c = spec();
        c.accel_mode = AccelMode::Compat;
        assert!(q
            .args(&c, false)
            .unwrap()
            .join(" ")
            .contains("-accel whpx,kernel-irqchip=off"));
        c.accel_mode = AccelMode::Software;
        assert!(q
            .args(&c, false)
            .unwrap()
            .join(" ")
            .contains("-accel tcg,thread=multi"));
        assert!(a.contains("ide-hd,drive=disk0"));
        let mut b = spec();
        b.display = Display::Basic;
        let b = q.args(&b, false).unwrap().join(" ");
        assert!(b.contains("-vga std -display "));
        assert!(!b.contains("virtio-vga"));
        assert!(a.contains("e1000e"));
        assert!(a.contains("-serial mon:stdio"));
        assert!(!a.contains("usb-tablet"));
    }
}
