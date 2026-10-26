// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Changing a VM after it was made: its hardware and display settings, the
//! size of its system disk, and snapshots of its disks. All of it only while
//! the VM is stopped.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::engine::{self, Disk, VmSpec};
use crate::host::Host;
use crate::image::DiskFormat;
use crate::ops::{parse_mib, parse_port, run_qemu_img};
use crate::preset::Preset;
use crate::store::{self, VmRecord};

/// Load a VM that may be changed: stopped, and not being diagnosed.
fn editable(name: &str) -> Result<VmRecord> {
    let rec = store::load(name)?;
    let hv = engine::backend(&rec.backend)?;
    if store::running(name, hv.as_ref()).is_some() {
        bail!("stop `{name}` before changing it");
    }
    if crate::diagnose::busy(name) {
        bail!("`{name}` is being diagnosed; wait for that to finish");
    }
    Ok(rec)
}

/// The changes to make; anything left out stays as it is.
#[derive(Debug, Default, Deserialize)]
pub struct Patch {
    pub cpus: Option<u32>,
    /// e.g. "16G".
    pub memory: Option<String>,
    /// HOST:GUEST[/udp] each; the whole list is replaced.
    pub ports: Option<Vec<String>>,
    pub fullscreen: Option<bool>,
    /// "1920x1080", or "" for the guest's own choice.
    pub resolution: Option<String>,
}

pub fn parse_resolution(s: &str) -> Result<Option<(u32, u32)>> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let (w, h) = s
        .split_once(['x', 'X'])
        .with_context(|| format!("resolution `{s}`: expected WIDTHxHEIGHT, e.g. 1920x1080"))?;
    let (w, h): (u32, u32) = (w.trim().parse()?, h.trim().parse()?);
    if !(640..=7680).contains(&w) || !(480..=4320).contains(&h) {
        bail!("resolution {w}x{h} is outside 640x480 to 7680x4320");
    }
    Ok(Some((w, h)))
}

/// Apply `patch` to the stopped VM `name`, checking it against this host
/// and the VM's engine the way `create` does.
pub fn apply(name: &str, host: &Host, patch: &Patch) -> Result<VmRecord> {
    let mut rec = editable(name)?;
    let native = rec.backend == "native";
    let spec = &mut rec.spec;
    if let Some(cpus) = patch.cpus {
        if cpus == 0 || cpus as usize > host.threads {
            bail!("vCPUs must be 1 to {} on this host", host.threads);
        }
        if native && cpus > 1 {
            bail!("bContain's own hypervisor runs 1 vCPU for now");
        }
        spec.cpus = cpus;
    }
    if let Some(m) = patch.memory.as_deref().filter(|m| !m.trim().is_empty()) {
        let mib = parse_mib(m)?;
        if mib < 256 {
            bail!("memory must be at least 256M");
        }
        if mib > host.memory_mib.saturating_sub(2048) {
            bail!(
                "{} MiB would leave the host less than 2 GiB of its {} MiB",
                mib,
                host.memory_mib
            );
        }
        if native && mib > crate::native::MAX_MEMORY_MIB {
            bail!(
                "bContain's own hypervisor takes at most {} MiB",
                crate::native::MAX_MEMORY_MIB
            );
        }
        spec.memory_mib = mib;
    }
    if let Some(ports) = &patch.ports {
        let ports = ports
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(parse_port)
            .collect::<Result<Vec<_>>>()?;
        if native && !ports.is_empty() {
            bail!("bContain's own hypervisor has no network card yet");
        }
        spec.ports = ports;
    }
    if let Some(f) = patch.fullscreen {
        spec.fullscreen = f;
    }
    if let Some(r) = &patch.resolution {
        spec.resolution = parse_resolution(r)?;
    }
    // Hugepages only while the reserved pool still covers the memory.
    let sizing = Preset::parse(&rec.preset).map(|p| p.sizing(host));
    rec.spec.hugepages =
        sizing.is_ok_and(|s| s.hugepages) && rec.spec.memory_mib <= host.hugepages_free_mib;
    store::save(&rec)?;
    Ok(rec)
}

/// The VM's main writable disk: the one a guest is installed on.
pub fn system_disk(spec: &VmSpec) -> Option<&Disk> {
    spec.disks
        .iter()
        .find(|d| !d.cdrom && !d.installer && !d.ephemeral)
}

/// Writable qcow2 disks: the ones a snapshot covers.
fn snapshot_disks(spec: &VmSpec) -> Vec<&Disk> {
    spec.disks
        .iter()
        .filter(|d| !d.cdrom && !d.ephemeral && d.format == DiskFormat::Qcow2)
        .collect()
}

#[derive(Debug, Deserialize)]
struct ImgInfo {
    #[serde(rename = "virtual-size")]
    virtual_size: u64,
    #[serde(default)]
    snapshots: Vec<ImgSnapshot>,
}

#[derive(Debug, Deserialize)]
struct ImgSnapshot {
    name: String,
    #[serde(rename = "date-sec")]
    date_sec: u64,
}

fn info(path: &Path) -> Result<ImgInfo> {
    // -U: read even if another process holds the image's lock.
    let text = run_qemu_img(&[
        OsStr::new("info"),
        OsStr::new("-U"),
        OsStr::new("--output=json"),
        path.as_os_str(),
    ])?;
    serde_json::from_str(&text).context("reading qemu-img info")
}

/// Grow the system disk to `size_mib`. Never shrinks: shrinking a disk
/// under a guest's partitions destroys data.
pub fn resize(name: &str, size_mib: u64) -> Result<(u64, u64)> {
    let rec = editable(name)?;
    let disk = system_disk(&rec.spec).with_context(|| format!("`{name}` has no disk to grow"))?;
    if !matches!(disk.format, DiskFormat::Qcow2 | DiskFormat::Raw) {
        bail!(
            "only qcow2 and raw disks can be grown here; this one is {}",
            disk.format.qemu_name()
        );
    }
    let now = info(&disk.path)?.virtual_size >> 20;
    if size_mib <= now {
        bail!(
            "the disk is already {} GiB; it can only grow (shrinking would cut off the guest's data)",
            now / 1024
        );
    }
    // Snapshots are fine: each keeps the size it was taken at, so restoring
    // an older one also brings back its smaller size.
    run_qemu_img(&[
        OsStr::new("resize"),
        OsStr::new("-q"),
        OsStr::new("-f"),
        OsStr::new(disk.format.qemu_name()),
        disk.path.as_os_str(),
        OsStr::new(&format!("{size_mib}M")),
    ])?;
    Ok((now, size_mib))
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Snapshot {
    pub name: String,
    /// Unix seconds.
    pub taken: u64,
}

fn check_snapshot_name(s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 40
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || " ._-".contains(c))
        && !s.starts_with([' ', '.', '-']);
    if !ok {
        bail!("snapshot names use letters, digits, spaces, '.', '_' and '-' (up to 40)");
    }
    Ok(())
}

/// Where a snapshot keeps the VM's UEFI variables (its boot entries), which
/// live outside the disks.
fn vars_copy(name: &str, snap: &str) -> PathBuf {
    store::vm_dir(name)
        .join("snapshots")
        .join(format!("{}.efivars.fd", snap.replace(' ', "_")))
}

pub fn snapshots(name: &str) -> Result<Vec<Snapshot>> {
    let rec = store::load(name)?;
    let Some(disk) = snapshot_disks(&rec.spec).first().copied() else {
        return Ok(Vec::new());
    };
    let mut list: Vec<Snapshot> = info(&disk.path)?
        .snapshots
        .into_iter()
        .map(|s| Snapshot {
            name: s.name,
            taken: s.date_sec,
        })
        .collect();
    list.sort_by_key(|s| s.taken);
    Ok(list)
}

/// Snapshots need qcow2: raw, VHDX and the like cannot hold them.
pub fn can_snapshot(spec: &VmSpec) -> bool {
    !snapshot_disks(spec).is_empty()
}

pub fn snapshot_create(name: &str, snap: &str) -> Result<()> {
    check_snapshot_name(snap)?;
    let rec = editable(name)?;
    let disks = snapshot_disks(&rec.spec);
    if disks.is_empty() {
        bail!("`{name}` has no qcow2 disk; snapshots need one");
    }
    if snapshots(name)?.iter().any(|s| s.name == snap) {
        bail!("`{name}` already has a snapshot called `{snap}`");
    }
    for d in &disks {
        run_qemu_img(&[
            OsStr::new("snapshot"),
            OsStr::new("-c"),
            OsStr::new(snap),
            d.path.as_os_str(),
        ])?;
    }
    let vars = store::vm_dir(name).join("efivars.fd");
    if vars.is_file() {
        let copy = vars_copy(name, snap);
        fs::create_dir_all(copy.parent().unwrap())?;
        fs::copy(&vars, &copy)?;
    }
    Ok(())
}

/// Put every disk back to `snap`. What changed since is lost.
pub fn snapshot_restore(name: &str, snap: &str) -> Result<()> {
    check_snapshot_name(snap)?;
    let rec = editable(name)?;
    if !snapshots(name)?.iter().any(|s| s.name == snap) {
        bail!("`{name}` has no snapshot called `{snap}`");
    }
    for d in snapshot_disks(&rec.spec) {
        run_qemu_img(&[
            OsStr::new("snapshot"),
            OsStr::new("-a"),
            OsStr::new(snap),
            d.path.as_os_str(),
        ])?;
    }
    let copy = vars_copy(name, snap);
    if copy.is_file() {
        fs::copy(&copy, store::vm_dir(name).join("efivars.fd"))?;
    }
    Ok(())
}

pub fn snapshot_delete(name: &str, snap: &str) -> Result<()> {
    check_snapshot_name(snap)?;
    let rec = editable(name)?;
    for d in snapshot_disks(&rec.spec) {
        run_qemu_img(&[
            OsStr::new("snapshot"),
            OsStr::new("-d"),
            OsStr::new(snap),
            d.path.as_os_str(),
        ])?;
    }
    let _ = fs::remove_file(vars_copy(name, snap));
    Ok(())
}

/// Everything the app's Settings panel shows for one VM.
#[derive(Debug, Serialize)]
pub struct Details {
    pub name: String,
    pub cpus: u32,
    pub memory_mib: u64,
    pub ports: Vec<String>,
    pub fullscreen: bool,
    pub resolution: String,
    pub has_display: bool,
    /// The system disk, if it has one.
    pub disk: Option<DiskDetails>,
    pub can_snapshot: bool,
    pub snapshots: Vec<Snapshot>,
    pub native: bool,
    pub host_threads: usize,
    pub host_memory_mib: u64,
}

#[derive(Debug, Serialize)]
pub struct DiskDetails {
    pub path: String,
    pub format: String,
    pub size_mib: u64,
    pub can_grow: bool,
}

pub fn details(name: &str, host: &Host) -> Result<Details> {
    let rec = store::load(name)?;
    let s = &rec.spec;
    let disk = system_disk(s).map(|d| {
        let can_grow = matches!(d.format, DiskFormat::Qcow2 | DiskFormat::Raw);
        DiskDetails {
            path: d.path.display().to_string(),
            format: d.format.qemu_name().to_string(),
            size_mib: info(&d.path).map(|i| i.virtual_size >> 20).unwrap_or(0),
            can_grow,
        }
    });
    Ok(Details {
        name: s.name.clone(),
        cpus: s.cpus,
        memory_mib: s.memory_mib,
        ports: s
            .ports
            .iter()
            .map(|p| format!("{}:{}{}", p.host, p.guest, if p.udp { "/udp" } else { "" }))
            .collect(),
        fullscreen: s.fullscreen,
        resolution: s
            .resolution
            .map(|(w, h)| format!("{w}x{h}"))
            .unwrap_or_default(),
        has_display: s.display != engine::Display::Headless,
        disk,
        can_snapshot: can_snapshot(s),
        snapshots: snapshots(name).unwrap_or_default(),
        native: rec.backend == "native",
        host_threads: host.threads,
        host_memory_mib: host.memory_mib,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolutions() {
        assert_eq!(parse_resolution("").unwrap(), None);
        assert_eq!(parse_resolution("1920x1080").unwrap(), Some((1920, 1080)));
        assert_eq!(
            parse_resolution(" 2560 X 1440 ").unwrap(),
            Some((2560, 1440))
        );
        assert!(parse_resolution("1920").is_err());
        assert!(parse_resolution("100x100").is_err());
    }

    #[test]
    fn snapshot_names() {
        assert!(check_snapshot_name("before update").is_ok());
        assert!(check_snapshot_name("v1.2_clean-install").is_ok());
        for bad in ["", "-a", "../x", "a/b", "x;rm", &"a".repeat(41)] {
            assert!(check_snapshot_name(bad).is_err(), "{bad}");
        }
    }
}
