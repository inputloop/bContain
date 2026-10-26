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
        })
    }
}

/// The QEMU command line configures eth0 with `ip=`; the native VMM has no
/// network card yet, and the kernel would wait for one.
fn native_cmdline(cmdline: &str) -> String {
    cmdline
        .split_whitespace()
        .filter(|p| !p.starts_with("ip="))
        .collect::<Vec<_>>()
        .join(" ")
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
        (bvmm::Outcome::Reset, None) => 0,
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
            native_cmdline(
                "console=ttyS0 rdinit=/x ip=10.0.2.15::10.0.2.2:255.255.255.0::eth0:off quiet"
            ),
            "console=ttyS0 rdinit=/x quiet"
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
