// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! bContain's own hypervisor (crates/bvmm) behind the same `Hypervisor`
//! trait as QEMU. It runs container micro-VMs: kernel + initramfs, serial
//! console. Disk images and ISOs stay on QEMU.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};

use crate::engine::{Hypervisor, Running, VmSpec};

/// The native VMM is single-vCPU and keeps RAM below the 3 GiB MMIO hole for now.
pub const MAX_MEMORY_MIB: u64 = 3072;

/// The line the container init prints when its command ends.
const EXIT_MARKER: &str = "[bcontain] container exited with code ";

pub struct Native;

impl Native {
    fn config(spec: &VmSpec) -> Result<bvmm::VmConfig> {
        let Some(k) = &spec.kernel else {
            bail!(
                "bContain's own hypervisor runs container images only for now; \
                 disk images and ISOs use the qemu engine"
            );
        };
        let kernel =
            std::fs::read(&k.kernel).with_context(|| format!("reading {}", k.kernel.display()))?;
        let initrd = match &k.initrd {
            Some(p) => Some(std::fs::read(p).with_context(|| format!("reading {}", p.display()))?),
            None => None,
        };
        Ok(bvmm::VmConfig {
            memory_mib: spec.memory_mib.min(MAX_MEMORY_MIB),
            kernel,
            initrd,
            cmdline: native_cmdline(&k.cmdline),
            disks: native_disks(std::env::var("BCONTAIN_NATIVE_DISKS").ok().as_deref()),
        })
    }
}

/// `BCONTAIN_NATIVE_DISKS` lists raw disk images for the native engine,
/// separated by `;`, each optionally ending in `,ro`. The first is
/// `/dev/vda` in the guest. For bring-up; the CLI does not offer it yet.
fn native_disks(spec: Option<&str>) -> Vec<bvmm::DiskConfig> {
    spec.unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| match s.strip_suffix(",ro") {
            Some(path) => bvmm::DiskConfig {
                path: path.into(),
                read_only: true,
            },
            None => bvmm::DiskConfig {
                path: s.into(),
                read_only: false,
            },
        })
        .collect()
}

/// The QEMU command line configures eth0 with `ip=`; the native VMM has no
/// network card yet, and the kernel would wait for one.
///
/// `BCONTAIN_KERNEL_ARGS`, when set, is appended, and the quiet flags are
/// dropped so the kernel's own messages show. It is for bring-up and
/// diagnosis: trying another kernel, or seeing where a boot stops.
fn native_cmdline(cmdline: &str) -> String {
    let extra = std::env::var("BCONTAIN_KERNEL_ARGS").ok();
    native_cmdline_with(cmdline, extra.as_deref())
}

fn native_cmdline_with(cmdline: &str, extra: Option<&str>) -> String {
    let extra = extra.map(str::trim).filter(|e| !e.is_empty());
    let mut parts: Vec<&str> = cmdline
        .split_whitespace()
        .filter(|p| !p.starts_with("ip="))
        .filter(|p| extra.is_none() || !(*p == "quiet" || p.starts_with("loglevel=")))
        .collect();
    if let Some(e) = extra {
        parts.extend(e.split_whitespace());
    }
    parts.join(" ")
}

/// Copies the guest console to stdout and remembers the container's exit code.
struct Console {
    out: Box<dyn Write + Send>,
    line: Vec<u8>,
    code: Arc<Mutex<Option<i32>>>,
}

impl Write for Console {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for &b in buf {
            if b == b'\n' {
                let line = String::from_utf8_lossy(&self.line);
                if let Some(rest) = line.trim_end_matches('\r').split(EXIT_MARKER).nth(1) {
                    if let Ok(c) = rest.trim().parse() {
                        *self.code.lock().unwrap() = Some(c);
                    }
                }
                self.line.clear();
            } else if self.line.len() < 4096 {
                self.line.push(b);
            }
        }
        self.out.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// Boot `spec` with the console on stdio. Returns the container's exit code.
pub fn run_attached(spec: &VmSpec, interactive: bool) -> Result<i32> {
    let cfg = Native::config(spec)?;
    let code = Arc::new(Mutex::new(None));
    let console = Console {
        out: Box::new(std::io::stdout()),
        line: Vec::new(),
        code: code.clone(),
    };
    let input: Option<Box<dyn std::io::Read + Send>> =
        interactive.then(|| Box::new(std::io::stdin()) as _);
    let outcome =
        bvmm::run_linux(&cfg, Box::new(console), input).map_err(|e| anyhow::anyhow!("{e}"))?;
    let code = *code.lock().unwrap();
    Ok(match (outcome, code) {
        (_, Some(c)) => c,
        (bvmm::Outcome::Reset, None) => {
            // Our init always reports an exit code before it reboots, so a
            // reset without one means the guest never got that far, or its
            // console is not reaching us.
            eprintln!(
                "bcontain: the guest reset without reporting an exit code \
                 (kernel panic before the console came up, or no serial console)"
            );
            1
        }
        (bvmm::Outcome::Halted, None) => {
            eprintln!("bcontain: the guest halted before the container reported an exit code");
            1
        }
    })
}

impl Hypervisor for Native {
    fn name(&self) -> &'static str {
        "native"
    }

    fn check(&self) -> Result<()> {
        let p = bvmm::probe();
        if !p.usable {
            bail!("{} is not available: {}", p.accel, p.detail);
        }
        Ok(())
    }

    /// Runs the VM in a child `bcontain` process so it outlives this one.
    fn launch(&self, spec: &VmSpec, log: &Path) -> Result<Running> {
        Native::config(spec)?;
        let out =
            std::fs::File::create(log).with_context(|| format!("creating {}", log.display()))?;
        let exe = std::env::current_exe()?;
        let mut cmd = Command::new(exe);
        cmd.args(["__native-run", &spec.name])
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0000_0200 | 0x0800_0000); // new process group, no window
        }
        let child = cmd.spawn().context("starting the native VM process")?;
        Ok(Running {
            pid: child.id(),
            control_port: 0,
        })
    }

    fn run_foreground(&self, spec: &VmSpec) -> Result<i32> {
        run_attached(spec, true)
    }

    /// There is no ACPI to request a clean shutdown yet: both stop the VM process.
    fn stop(&self, running: &Running, _force: bool) -> Result<()> {
        let mut sys = sysinfo::System::new();
        let pid = sysinfo::Pid::from_u32(running.pid);
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        match sys.process(pid) {
            Some(p) if p.kill() => Ok(()),
            Some(_) => bail!("could not stop process {}", running.pid),
            None => Ok(()),
        }
    }

    fn is_alive(&self, running: &Running) -> bool {
        let mut sys = sysinfo::System::new();
        let pid = sysinfo::Pid::from_u32(running.pid);
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        sys.process(pid).is_some_and(|p| {
            !matches!(
                p.status(),
                sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
            ) && p
                .name()
                .to_string_lossy()
                .to_lowercase()
                .contains("bcontain")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_ip_param() {
        assert_eq!(
            native_cmdline_with(
                "console=ttyS0 rdinit=/x ip=10.0.2.15::10.0.2.2:255.255.255.0::eth0:off quiet",
                None
            ),
            "console=ttyS0 rdinit=/x quiet"
        );
    }

    #[test]
    fn extra_kernel_args_replace_the_quiet_flags() {
        assert_eq!(
            native_cmdline_with(
                "console=ttyS0 quiet loglevel=3 ip=x",
                Some(" ignore_loglevel  tsc_early_khz=2000000 ")
            ),
            "console=ttyS0 ignore_loglevel tsc_early_khz=2000000"
        );
        assert_eq!(
            native_cmdline_with("console=ttyS0 quiet loglevel=3", Some("  ")),
            "console=ttyS0 quiet loglevel=3"
        );
    }

    #[test]
    fn parses_native_disks() {
        assert!(native_disks(None).is_empty());
        assert!(native_disks(Some(" ; ")).is_empty());
        let d = native_disks(Some("a.img; C:\\vm\\b.img,ro"));
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].path.to_str(), d[0].read_only), (Some("a.img"), false));
        assert_eq!(
            (d[1].path.to_str(), d[1].read_only),
            (Some("C:\\vm\\b.img"), true)
        );
    }

    #[test]
    fn reads_exit_code() {
        let code = Arc::new(Mutex::new(None));
        let mut c = Console {
            out: Box::new(std::io::sink()),
            line: Vec::new(),
            code: code.clone(),
        };
        c.write_all(b"hello\r\n[bcontain] container exi").unwrap();
        c.write_all(b"ted with code 3\r\n[  2.1] reboot\r\n")
            .unwrap();
        assert_eq!(*code.lock().unwrap(), Some(3));
    }
}
