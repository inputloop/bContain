// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! `bcontain diagnose`: find which part of a VM's virtual hardware stops it
//! before its firmware draws anything ("Guest has not initialized the
//! display"). Boots the VM once per variant, without a window, screenshots
//! it over QMP, and reports which variants reach the firmware's screen.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::engine::{AccelMode, DiskBus, Display, Firmware, GuestOs, VmSpec};
use crate::qemu::{self, Qemu};
use crate::store;

/// What a probe saw.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The guest set a video mode (OVMF draws at 1280x800, SeaBIOS text 720x400).
    Drew(u32, u32),
    /// QEMU's 640x480 "Guest has not initialized the display" placeholder.
    NothingDrawn,
    Exited(String),
    /// QEMU stopped answering its control channel (it was then ended).
    Unresponsive,
    /// QEMU stopped answering and could not be ended: frozen in the hypervisor.
    Frozen(u32),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Drew(w, h) => write!(f, "OK: firmware drew the screen ({w}x{h})"),
            Outcome::NothingDrawn => write!(f, "STUCK: nothing drawn"),
            Outcome::Exited(e) => write!(f, "QEMU EXITED: {e}"),
            Outcome::Unresponsive => write!(f, "STUCK: QEMU itself stopped responding"),
            Outcome::Frozen(pid) => write!(
                f,
                "FROZE: QEMU stopped responding and would not quit (end qemu-system-x86_64.exe, pid {pid}, in Task Manager)"
            ),
        }
    }
}

/// Width and height from a binary PPM (P6) header.
fn ppm_size(data: &[u8]) -> Option<(u32, u32)> {
    let text = String::from_utf8_lossy(&data[..data.len().min(64)]);
    let mut fields = text.split_ascii_whitespace();
    if fields.next()? != "P6" {
        return None;
    }
    Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
}

fn classify(size: Option<(u32, u32)>) -> Outcome {
    match size {
        Some((640, 480)) | None => Outcome::NothingDrawn,
        Some((w, h)) => Outcome::Drew(w, h),
    }
}

fn mode_label(mode: AccelMode) -> &'static str {
    match mode {
        AccelMode::Auto => "automatic",
        AccelMode::Compat => "compatibility",
        AccelMode::Software => "software",
    }
}

/// Round one: the VM as configured, then each other acceleration mode.
fn mode_variants(spec: &VmSpec) -> Vec<(String, VmSpec)> {
    let mut out = vec![("as configured".to_string(), spec.clone())];
    for (mode, label) in [
        (AccelMode::Auto, "acceleration: automatic"),
        (AccelMode::Compat, "acceleration: compatibility"),
        (AccelMode::Software, "acceleration: software (slow)"),
    ] {
        if spec.accel_mode != mode {
            let mut s = spec.clone();
            s.accel_mode = mode;
            out.push((label.into(), s));
        }
    }
    out
}

/// Round two: `base` (a mode that got stuck) without each piece of its
/// hardware in turn. Trying these in a mode that works would only confirm
/// that it works.
fn hardware_variants(base: &VmSpec) -> Vec<(String, VmSpec)> {
    let spec = base;
    let mut out = Vec::new();
    let mut add = |label: &str, change: &dyn Fn(&mut VmSpec) -> bool| {
        let mut s = spec.clone();
        if change(&mut s) {
            out.push((label.into(), s));
        }
    };
    add("BIOS firmware instead of UEFI", &|s| {
        std::mem::replace(&mut s.firmware, Firmware::Bios) == Firmware::Uefi
    });
    add("without Windows settings (local-time clock)", &|s| {
        std::mem::replace(&mut s.guest, GuestOs::Other) == GuestOs::Windows
    });
    add("without the answer disc", &|s| {
        let before = s.disks.len();
        s.disks.retain(|d| !d.path.ends_with("autounattend.iso"));
        s.disks.len() != before
    });
    add("without any CD", &|s| {
        let before = s.disks.len();
        s.disks.retain(|d| !d.cdrom);
        s.disks.len() != before
    });
    add("SATA system disk instead of NVMe", &|s| {
        let mut changed = false;
        for d in s.disks.iter_mut().filter(|d| d.bus == DiskBus::Nvme) {
            d.bus = DiskBus::Auto;
            changed = true;
        }
        s.legacy_devices |= changed;
        changed
    });
    add("virtio network instead of e1000e", &|s| {
        // legacy_devices also picks e1000e; an NVMe disk is unaffected.
        let nvme_only = s.disks.iter().all(|d| d.cdrom || d.bus != DiskBus::Auto);
        std::mem::replace(&mut s.legacy_devices, false) && nvme_only
    });
    add("1 vCPU", &|s| std::mem::replace(&mut s.cpus, 1) > 1);
    add("4 GiB of memory", &|s| {
        s.hugepages = false;
        std::mem::replace(&mut s.memory_mib, 4096) > 4096
    });
    add("without an explicit boot order", &|s| {
        let mut changed = false;
        for d in s.disks.iter_mut().filter(|d| d.installer) {
            d.installer = false;
            changed = true;
        }
        changed
    });
    out
}

/// Boot `spec` without a window for a few seconds and screenshot it.
fn probe(q: &Qemu, spec: &VmSpec, work: &Path) -> Result<Outcome> {
    // No window means no OpenGL: virgl's device refuses to start without it.
    let mut spec = spec.clone();
    if spec.display == Display::Virgl {
        spec.display = Display::Basic;
    }
    let mut args = q.args(&spec, false)?;
    if let Some(i) = args.iter().position(|a| a == "-display") {
        args[i + 1] = "none".into();
    }
    let port = qemu::free_port()?;
    args.extend([
        "-qmp".into(),
        format!("tcp:127.0.0.1:{port},server=on,wait=off"),
    ]);
    let log = work.join("diagnose.log");
    let out = std::fs::File::create(&log)?;
    let mut child = q
        .background_command(&args)
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .spawn()
        .context("starting qemu")?;
    let spec = &spec;
    let wait = if spec.accel_mode == AccelMode::Software {
        60
    } else {
        15
    };
    let end = Instant::now() + Duration::from_secs(wait);
    while Instant::now() < end {
        if child.try_wait()?.is_some() {
            let last = qemu::tail(&log, 3);
            return Ok(Outcome::Exited(last.trim().replace('\n', " | ")));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let shot = work.join("diagnose.ppm");
    let _ = std::fs::remove_file(&shot);
    // A QEMU frozen inside the hypervisor may never answer, or never exit:
    // every step from here has a deadline, so one variant cannot stall the
    // rest of the diagnosis.
    let (tx, rx) = std::sync::mpsc::channel();
    let target = shot.clone();
    std::thread::spawn(move || {
        let taken = qemu::qmp_with(
            port,
            "screendump",
            Some(serde_json::json!({ "filename": target.to_string_lossy() })),
        );
        let _ = qemu::qmp_with(port, "quit", None);
        let _ = tx.send(taken.map_err(|e| format!("{e:#}")));
    });
    let taken = rx
        .recv_timeout(Duration::from_secs(15))
        .unwrap_or_else(|_| Err("QMP did not answer".into()));
    let _ = child.kill();
    let gone = Instant::now() + Duration::from_secs(10);
    while child.try_wait()?.is_none() {
        if Instant::now() > gone {
            return Ok(Outcome::Frozen(child.id()));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if taken.is_err() && !shot.exists() {
        return Ok(Outcome::Unresponsive);
    }
    Ok(classify(
        std::fs::read(&shot).ok().as_deref().and_then(ppm_size),
    ))
}

/// `run`'s progress, for the app, which polls it: a diagnosis takes minutes.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Job {
    pub lines: Vec<String>,
    /// The variant being tried right now.
    pub trying: Option<String>,
    pub done: bool,
}

static JOBS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Job>>> =
    std::sync::LazyLock::new(Default::default);

fn update(name: &str, f: impl FnOnce(&mut Job)) {
    if let Ok(mut jobs) = JOBS.lock() {
        f(jobs.entry(name.to_string()).or_default());
    }
}

/// Start diagnosing `name` on a background thread (once at a time per VM).
pub fn start_background(name: &str) {
    if let Ok(mut jobs) = JOBS.lock() {
        if jobs.get(name).is_some_and(|j| !j.done) {
            return;
        }
        jobs.insert(name.to_string(), Job::default());
    }
    let name = name.to_string();
    std::thread::spawn(move || {
        let result = run(
            &name,
            &mut |l| update(&name, |j| j.lines.push(l)),
            &mut |t| update(&name, |j| j.trying = Some(t.to_string())),
        );
        update(&name, |j| {
            if let Err(e) = result {
                j.lines.push(format!("error: {e:#}"));
            }
            j.trying = None;
            j.done = true;
        });
    });
}

/// The latest progress of a background diagnosis, if one was started.
pub fn status(name: &str) -> Option<Job> {
    JOBS.lock().ok()?.get(name).cloned()
}

/// A diagnosis is running: its probes have the VM's disks open.
pub fn busy(name: &str) -> bool {
    status(name).is_some_and(|j| !j.done)
}

pub fn run(name: &str, report: &mut dyn FnMut(String), trying: &mut dyn FnMut(&str)) -> Result<()> {
    let rec = store::load(name)?;
    if rec.backend != "qemu" {
        bail!("diagnose works on QEMU VMs");
    }
    if rec.spec.gpu.is_some() {
        bail!("diagnose cannot run a VM with GPU passthrough");
    }
    if rec.spec.display == Display::Headless {
        bail!("`{name}` has no display to check");
    }
    let q = Qemu::locate()?;
    if store::running(name, &q).is_some() {
        bail!("stop `{name}` first");
    }
    let work = store::vm_dir(name);
    let mut run_round = |round: &[(String, VmSpec)], report: &mut dyn FnMut(String)| {
        let mut stuck = Vec::new();
        for (label, spec) in round {
            trying(label);
            let outcome =
                probe(&q, spec, &work).unwrap_or_else(|e| Outcome::Exited(format!("{e:#}")));
            if !matches!(outcome, Outcome::Drew(..)) {
                stuck.push(spec.clone());
            }
            report(format!("  {label:<46} {outcome}"));
        }
        stuck
    };
    let modes = mode_variants(&rec.spec);
    report(format!(
        "Booting `{name}` without a window, 15 s each (60 s in software mode). Round 1, acceleration modes ({}):",
        modes.len()
    ));
    let stuck = run_round(&modes, report);
    // Diagnose the mode the VM is set to if that is stuck, else the first
    // other stuck mode (automatic, normally): that is the one worth fixing.
    let Some(base) = stuck.first() else {
        report("Every mode reaches the firmware: the VM stops later, inside the guest. Watch its window for where.".into());
        return Ok(());
    };
    if base.accel_mode == AccelMode::Software {
        report("Only software mode is stuck; the hardware modes work, so use one of those.".into());
        return Ok(());
    }
    let hw = hardware_variants(base);
    report(format!(
        "Round 2, {} mode without each piece of hardware in turn ({}):",
        mode_label(base.accel_mode),
        hw.len()
    ));
    let still = run_round(&hw, report);
    if still.len() == hw.len() {
        report(format!(
            "Nothing helps: {} mode itself stops this VM before the firmware on this machine. Use another mode.",
            mode_label(base.accel_mode)
        ));
    } else {
        report("The OK lines name what to change for this mode to work.".into());
    }
    report("Send this table to whoever is helping you; nothing about the VM was changed.".into());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_screenshots() {
        assert_eq!(ppm_size(b"P6\n1280 800\n255\n\0\0\0"), Some((1280, 800)));
        assert_eq!(classify(Some((640, 480))), Outcome::NothingDrawn);
        assert_eq!(classify(Some((720, 400))), Outcome::Drew(720, 400));
        assert_eq!(classify(None), Outcome::NothingDrawn);
    }
}
