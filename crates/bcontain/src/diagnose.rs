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
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Drew(w, h) => write!(f, "OK: firmware drew the screen ({w}x{h})"),
            Outcome::NothingDrawn => write!(f, "STUCK: nothing drawn"),
            Outcome::Exited(e) => write!(f, "QEMU EXITED: {e}"),
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

/// The variants to try, each a change to the VM as configured.
fn variants(spec: &VmSpec) -> Vec<(String, VmSpec)> {
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
    let mut args = q.args(spec, false)?;
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
    let taken = qemu::qmp_with(
        port,
        "screendump",
        Some(serde_json::json!({ "filename": shot.to_string_lossy() })),
    );
    let _ = qemu::qmp_with(port, "quit", None);
    std::thread::sleep(Duration::from_millis(500));
    let _ = child.kill();
    let _ = child.wait();
    taken.context("taking a screenshot over QMP")?;
    Ok(classify(
        std::fs::read(&shot).ok().as_deref().and_then(ppm_size),
    ))
}

pub fn run(name: &str, report: &mut dyn FnMut(String)) -> Result<()> {
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
    let all = variants(&rec.spec);
    report(format!(
        "Booting `{name}` {} ways without a window (15 s each, 60 s in software mode):",
        all.len()
    ));
    for (label, spec) in &all {
        let outcome = probe(&q, spec, &work).unwrap_or_else(|e| Outcome::Exited(format!("{e:#}")));
        report(format!("  {label:<46} {outcome}"));
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
