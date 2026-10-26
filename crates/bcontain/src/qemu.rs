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

use crate::engine::{Display, Firmware, Hypervisor, Running, VmSpec};

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
        let out = Command::new(&self.binary).arg("--version").output().ok()?;
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
        self.share_dirs()
            .iter()
            .flat_map(|d| NAMES.iter().map(move |n| d.join(n)))
            .find(|p| p.is_file())
    }

    /// Build the full QEMU argument list for `spec`.
    pub fn args(&self, spec: &VmSpec, foreground: bool) -> Result<Vec<String>> {
        let mut a: Vec<String> = Vec::new();
        let mut push = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));

        push(&["-name", &spec.name]);
        let machine = if spec.hugepages {
            "q35,memory-backend=ram0"
        } else {
            "q35"
        };
        push(&["-machine", machine]);
        match self.accel {
            // WHPX's in-kernel irqchip is unreliable across Windows builds.
            Accel::Whp => push(&["-accel", "whpx,kernel-irqchip=off", "-cpu", "max"]),
            Accel::Kvm => push(&["-accel", "kvm", "-cpu", "host"]),
            Accel::None => push(&["-accel", "tcg,thread=multi", "-cpu", "max"]),
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
        }

        let mut has_cdrom = false;
        for (i, d) in spec.disks.iter().enumerate() {
            if d.cdrom {
                has_cdrom = true;
                let drive = format!(
                    "file={},format=raw,media=cdrom,readonly=on,if=none,id=cd{i}",
                    qemu_path(&d.path)
                );
                let dev = format!("ide-cd,drive=cd{i}");
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
            let dev = if spec.legacy_devices {
                format!("ide-hd,drive=disk{i}")
            } else {
                format!("virtio-blk-pci,drive=disk{i}")
            };
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
            netdev.push_str(&format!(",hostfwd=tcp:127.0.0.1:{}-:{}", p.host, p.guest));
        }
        push(&["-netdev", &netdev]);
        let nic = if spec.legacy_devices {
            "e1000e,netdev=net0"
        } else {
            "virtio-net-pci,netdev=net0"
        };
        push(&["-device", nic]);
        push(&["-device", "virtio-rng-pci"]);

        let gui_backend = if cfg!(windows) { "sdl" } else { "gtk" };
        match spec.display {
            Display::Virgl => {
                let disp = format!("{gui_backend},gl=on");
                push(&["-device", "virtio-vga-gl", "-display", &disp]);
            }
            Display::Basic => push(&["-device", "virtio-vga", "-display", gui_backend]),
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
            push(&["-device", "qemu-xhci", "-device", "usb-tablet"]);
        }
        if spec.no_reboot {
            push(&["-no-reboot"]);
        }
        Ok(a)
    }

    fn command(&self, args: &[String]) -> Command {
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
        let mut cmd = self.command(&args);
        cmd.stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        }
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
            for _ in 0..50 {
                if !self.is_alive(running) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            bail!("qemu (pid {}) did not exit after `quit`", running.pid);
        }
        Ok(())
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
    let stream = TcpStream::connect(("127.0.0.1", port))
        .with_context(|| format!("connecting to QMP on port {port}; is the VM running?"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut w = stream;
    let mut line = String::new();
    reader.read_line(&mut line)?; // greeting
    for cmd in ["qmp_capabilities", command] {
        writeln!(w, "{{\"execute\":\"{cmd}\"}}")?;
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

/// QEMU option values use `,` as a separator; a literal comma is written `,,`.
fn qemu_path(p: &Path) -> String {
    p.to_string_lossy().replace(',', ",,")
}

fn free_port() -> Result<u16> {
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
                },
                Disk {
                    path: "/iso/x.iso".into(),
                    format: DiskFormat::Iso,
                    cdrom: true,
                    ephemeral: false,
                },
            ],
            kernel: None,
            display: Display::Virgl,
            firmware: Firmware::Bios,
            ports: vec![PortForward {
                host: 8080,
                guest: 80,
            }],
            hugepages: false,
            no_reboot: false,
            legacy_devices: false,
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
        assert!(a.contains("hostfwd=tcp:127.0.0.1:8080-:80"));
        assert!(a.contains("virtio-vga-gl"));
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
        assert!(a.contains("whpx,kernel-irqchip=off"));
        assert!(a.contains("ide-hd,drive=disk0"));
        assert!(a.contains("e1000e"));
        assert!(a.contains("-serial mon:stdio"));
        assert!(!a.contains("usb-tablet"));
    }
}
