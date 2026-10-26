// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! GPU passthrough (Linux hosts): find the GPUs, check that one can be handed
//! to a VM with VFIO, bind it to `vfio-pci` while the VM runs, and give it
//! back afterwards. Everything is read from sysfs, so the checks run without
//! root; binding needs root.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

const NVIDIA: &str = "0x10de";

/// Where PCI devices live. Tests point this at a fake tree.
#[derive(Clone)]
pub struct Sysfs {
    pub root: PathBuf,
}

impl Default for Sysfs {
    fn default() -> Self {
        Sysfs {
            root: PathBuf::from("/sys"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PciFunction {
    pub address: String,
    pub vendor: String,
    pub device: String,
    pub class: String,
    pub driver: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Gpu {
    pub address: String,
    pub name: String,
    pub vendor: String,
    pub device: String,
    pub subsystem_vendor: String,
    pub subsystem_device: String,
    pub driver: Option<String>,
    /// The firmware's boot display (the screen you see). Laptops with a
    /// hybrid (Optimus) setup boot on the Intel iGPU, leaving the dGPU free.
    pub boot_vga: bool,
    pub iommu_group: Option<u32>,
    /// Everything in the IOMMU group, which must all go to the VM together.
    pub group: Vec<PciFunction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub item: &'static str,
    pub ok: bool,
    pub detail: String,
    pub fix: Option<String>,
}

fn read(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn driver_of(dev: &Path) -> Option<String> {
    fs::read_link(dev.join("driver"))
        .ok()?
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

fn function(dev: &Path) -> PciFunction {
    PciFunction {
        address: dev
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        vendor: read(&dev.join("vendor")).unwrap_or_default(),
        device: read(&dev.join("device")).unwrap_or_default(),
        class: read(&dev.join("class")).unwrap_or_default(),
        driver: driver_of(dev),
    }
}

fn friendly_name(vendor: &str, device: &str) -> String {
    match vendor {
        "0x10de" => format!("NVIDIA GPU {}", device.trim_start_matches("0x")),
        "0x8086" => format!("Intel GPU {}", device.trim_start_matches("0x")),
        "0x1002" => format!("AMD GPU {}", device.trim_start_matches("0x")),
        _ => format!("GPU {vendor}:{device}"),
    }
}

impl Sysfs {
    fn devices(&self) -> PathBuf {
        self.root.join("bus/pci/devices")
    }

    /// Whether the kernel has an IOMMU active (VT-d on and enabled in Linux).
    pub fn iommu_enabled(&self) -> bool {
        fs::read_dir(self.root.join("kernel/iommu_groups")).is_ok_and(|mut d| d.next().is_some())
    }

    pub fn gpus(&self) -> Vec<Gpu> {
        let Ok(entries) = fs::read_dir(self.devices()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for e in entries.flatten() {
            let dev = e.path();
            let class = read(&dev.join("class")).unwrap_or_default();
            // 0x0300xx VGA, 0x0302xx 3D controller (how laptops list the dGPU).
            if !(class.starts_with("0x0300") || class.starts_with("0x0302")) {
                continue;
            }
            let f = function(&dev);
            let group_dir = dev.join("iommu_group");
            let iommu_group = fs::read_link(&group_dir)
                .ok()
                .and_then(|p| p.file_name()?.to_str()?.parse().ok());
            let mut group: Vec<PciFunction> = fs::read_dir(group_dir.join("devices"))
                .map(|it| {
                    it.flatten()
                        .map(|m| function(&self.devices().join(m.file_name())))
                        .collect()
                })
                .unwrap_or_default();
            group.sort_by(|a, b| a.address.cmp(&b.address));
            out.push(Gpu {
                name: friendly_name(&f.vendor, &f.device),
                address: f.address.clone(),
                vendor: f.vendor.clone(),
                device: f.device.clone(),
                subsystem_vendor: read(&dev.join("subsystem_vendor")).unwrap_or_default(),
                subsystem_device: read(&dev.join("subsystem_device")).unwrap_or_default(),
                driver: f.driver,
                boot_vga: read(&dev.join("boot_vga")).as_deref() == Some("1"),
                iommu_group,
                group,
            });
        }
        out.sort_by(|a, b| a.address.cmp(&b.address));
        out
    }

    /// The GPU to pass through: the NVIDIA one that is not driving the screen.
    pub fn passthrough_candidate(&self) -> Option<Gpu> {
        let gpus = self.gpus();
        gpus.iter()
            .find(|g| g.vendor == NVIDIA && !g.boot_vga)
            .or_else(|| gpus.iter().find(|g| !g.boot_vga))
            .cloned()
    }
}

/// PCI bridges and root ports may share the group; they stay with the host.
fn is_bridge(f: &PciFunction) -> bool {
    f.class.starts_with("0x0604")
}

/// The functions that must be handed to the VM: the group minus bridges.
pub fn passthrough_functions(gpu: &Gpu) -> Vec<String> {
    let mut v: Vec<String> = gpu
        .group
        .iter()
        .filter(|f| !is_bridge(f))
        .map(|f| f.address.clone())
        .collect();
    if v.is_empty() {
        v.push(gpu.address.clone());
    }
    v
}

/// Everything that stands between this host and passing `gpu` to a VM.
pub fn readiness(sys: &Sysfs, gpu: Option<&Gpu>) -> Vec<Check> {
    let mut checks = Vec::new();
    let linux = cfg!(target_os = "linux");
    checks.push(Check {
        item: "Host OS",
        ok: linux,
        detail: if linux { "Linux".into() } else { std::env::consts::OS.into() },
        fix: (!linux).then(|| {
            "GPU passthrough needs a Linux host: Windows Hypervisor Platform cannot give a GPU to a \
             VM. See docs/GPU-PASSTHROUGH.md for setting up Linux next to Windows."
                .into()
        }),
    });
    if !linux {
        return checks;
    }
    let kvm = bvmm::probe();
    checks.push(Check {
        item: "KVM",
        ok: kvm.usable,
        detail: if kvm.usable {
            kvm.detail.clone()
        } else {
            "not available".into()
        },
        fix: (!kvm.usable).then(|| kvm.detail.clone()),
    });
    let iommu = sys.iommu_enabled();
    checks.push(Check {
        item: "IOMMU",
        ok: iommu,
        detail: if iommu { "active".into() } else { "not active".into() },
        fix: (!iommu).then(|| {
            "enable VT-d in the laptop's BIOS/UEFI setup, then add `intel_iommu=on iommu=pt` to the \
             kernel command line and reboot"
                .into()
        }),
    });
    let Some(g) = gpu else {
        checks.push(Check {
            item: "GPU",
            ok: false,
            detail: "no GPU that is free of the host display".into(),
            fix: Some(
                "set the laptop's GPU mode to Hybrid/Optimus (MUX switch) so the Intel GPU drives the \
                 screen and the NVIDIA GPU is free"
                    .into(),
            ),
        });
        return checks;
    };
    checks.push(Check {
        item: "GPU",
        ok: !g.boot_vga,
        detail: format!("{} at {} (driver {})", g.name, g.address, g.driver.as_deref().unwrap_or("none")),
        fix: g.boot_vga.then(|| {
            "this GPU drives the screen; set the GPU mode to Hybrid/Optimus (MUX switch) so the Intel \
             GPU takes over"
                .into()
        }),
    });
    let foreign: Vec<&PciFunction> = g
        .group
        .iter()
        .filter(|f| {
            !is_bridge(f)
                && !f
                    .address
                    .starts_with(g.address.trim_end_matches(|c| c != '.'))
        })
        .collect();
    checks.push(Check {
        item: "IOMMU group",
        ok: g.iommu_group.is_some() && foreign.is_empty(),
        detail: match g.iommu_group {
            Some(n) => format!(
                "group {n}: {}",
                g.group.iter().map(|f| f.address.as_str()).collect::<Vec<_>>().join(", ")
            ),
            None => "no IOMMU group".into(),
        },
        fix: if g.iommu_group.is_none() {
            Some("turn on the IOMMU (see above)".into())
        } else if !foreign.is_empty() {
            Some(format!(
                "the group also holds {}, which would have to go to the VM too; this machine's PCIe \
                 layout does not isolate the GPU",
                foreign.iter().map(|f| f.address.as_str()).collect::<Vec<_>>().join(", ")
            ))
        } else {
            None
        },
    });
    let memlock = memlock_limit_mib();
    checks.push(Check {
        item: "Locked memory",
        ok: memlock.is_none_or(|m| m >= 16 * 1024),
        detail: match memlock {
            None => "unlimited".into(),
            Some(m) => format!("{m} MiB"),
        },
        fix: memlock.filter(|m| *m < 16 * 1024).map(|_| {
            "VFIO pins all guest RAM: run the VM as root (sudo), or raise `memlock` for your user in \
             /etc/security/limits.conf"
                .into()
        }),
    });
    checks
}

/// RLIMIT_MEMLOCK of this process in MiB (`None` = unlimited).
fn memlock_limit_mib() -> Option<u64> {
    let limits = fs::read_to_string("/proc/self/limits").ok()?;
    let line = limits
        .lines()
        .find(|l| l.starts_with("Max locked memory"))?;
    let soft = line.split_whitespace().nth(3)?;
    if soft == "unlimited" {
        None
    } else {
        soft.parse::<u64>().ok().map(|b| b / (1024 * 1024))
    }
}

/// Processes that hold the NVIDIA device files open. They must close before
/// the GPU can leave the nvidia driver (a desktop running on the dGPU, CUDA
/// jobs, nvidia-persistenced...).
pub fn nvidia_users() -> Vec<String> {
    let mut users = Vec::new();
    let Ok(procs) = fs::read_dir("/proc") else {
        return users;
    };
    for p in procs.flatten() {
        let pid = p.file_name().to_string_lossy().into_owned();
        if !pid.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = fs::read_dir(p.path().join("fd")) else {
            continue;
        };
        let holds = fds.flatten().any(|fd| {
            fs::read_link(fd.path()).is_ok_and(|t| t.to_string_lossy().starts_with("/dev/nvidia"))
        });
        if holds {
            let comm = read(&p.path().join("comm")).unwrap_or_default();
            users.push(format!("{comm} (pid {pid})"));
        }
    }
    users
}

fn write(path: &Path, value: &str) -> Result<()> {
    fs::write(path, value).with_context(|| format!("writing `{value}` to {}", path.display()))
}

fn require_root() -> Result<()> {
    #[cfg(unix)]
    {
        if !std::env::var("USER").is_ok_and(|u| u == "root")
            && fs::metadata("/proc/self")
                .map(|m| std::os::unix::fs::MetadataExt::uid(&m))
                .unwrap_or(1)
                != 0
        {
            bail!("binding a GPU needs root: run this with sudo");
        }
    }
    Ok(())
}

/// Hand `functions` to vfio-pci (unbinding them from their current drivers).
pub fn bind_vfio(sys: &Sysfs, functions: &[String]) -> Result<()> {
    require_root()?;
    let users = nvidia_users();
    if !users.is_empty() {
        bail!(
            "the NVIDIA GPU is in use by {}; close those programs (or switch the desktop to the Intel \
             GPU) first",
            users.join(", ")
        );
    }
    let _ = std::process::Command::new("modprobe")
        .arg("vfio-pci")
        .status();
    for f in functions {
        let dev = sys.devices().join(f);
        write(&dev.join("driver_override"), "vfio-pci")?;
        if driver_of(&dev).is_some_and(|d| d != "vfio-pci") {
            write(&dev.join("driver/unbind"), f)?;
        }
        if driver_of(&dev).is_none() {
            write(&sys.root.join("bus/pci/drivers_probe"), f)?;
        }
        if driver_of(&dev).as_deref() != Some("vfio-pci") {
            bail!("{f} did not bind to vfio-pci (is the vfio-pci module available?)");
        }
    }
    Ok(())
}

/// Give `functions` back to their normal drivers.
pub fn unbind_vfio(sys: &Sysfs, functions: &[String]) -> Result<()> {
    require_root()?;
    for f in functions {
        let dev = sys.devices().join(f);
        write(&dev.join("driver_override"), "\n")?;
        if driver_of(&dev).as_deref() == Some("vfio-pci") {
            write(&dev.join("driver/unbind"), f)?;
        }
        write(&sys.root.join("bus/pci/drivers_probe"), f)?;
    }
    Ok(())
}

/// Dump the GPU's video BIOS through sysfs. Laptop GPUs often keep it in
/// system firmware instead of on the card, in which case this fails and the
/// guide's alternative applies.
pub fn dump_vbios(sys: &Sysfs, gpu: &str, out: &Path) -> Result<usize> {
    require_root()?;
    let rom = sys.devices().join(gpu).join("rom");
    write(&rom, "1")?;
    let data = fs::read(&rom);
    let _ = write(&rom, "0");
    let data = data.with_context(|| {
        format!("{gpu} exposes no readable ROM (common on laptops; see docs/GPU-PASSTHROUGH.md)")
    })?;
    if data.len() < 512 || data[0] != 0x55 || data[1] != 0xaa {
        bail!("{gpu}'s ROM does not start with a valid option-ROM signature");
    }
    fs::write(out, &data)?;
    Ok(data.len())
}

#[cfg(all(test, unix))]
pub mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A fake /sys modelled on an Optimus laptop: Intel iGPU drives the
    /// screen; NVIDIA dGPU (+ HDA audio) alone in IOMMU group 13 behind a root port.
    pub fn laptop_sysfs(dir: &Path, dgpu_group_extra: Option<&str>) -> Sysfs {
        let devs = dir.join("bus/pci/devices");
        let drivers = dir.join("bus/pci/drivers");
        let groups = dir.join("kernel/iommu_groups");
        let mk = |addr: &str,
                  vendor: &str,
                  device: &str,
                  class: &str,
                  driver: Option<&str>,
                  group: u32,
                  boot_vga: Option<&str>| {
            let d = devs.join(addr);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("vendor"), format!("{vendor}\n")).unwrap();
            fs::write(d.join("device"), format!("{device}\n")).unwrap();
            fs::write(d.join("class"), format!("{class}\n")).unwrap();
            fs::write(d.join("subsystem_vendor"), "0x1043\n").unwrap();
            fs::write(d.join("subsystem_device"), "0x16c3\n").unwrap();
            if let Some(b) = boot_vga {
                fs::write(d.join("boot_vga"), format!("{b}\n")).unwrap();
            }
            if let Some(drv) = driver {
                fs::create_dir_all(drivers.join(drv)).unwrap();
                symlink(drivers.join(drv), d.join("driver")).unwrap();
            }
            let g = groups.join(group.to_string());
            fs::create_dir_all(g.join("devices")).unwrap();
            symlink(&g, d.join("iommu_group")).unwrap();
            symlink(&d, g.join("devices").join(addr)).unwrap();
        };
        mk(
            "0000:00:02.0",
            "0x8086",
            "0x46a6",
            "0x030000",
            Some("i915"),
            0,
            Some("1"),
        );
        mk(
            "0000:00:01.0",
            "0x8086",
            "0x460d",
            "0x060400",
            Some("pcieport"),
            13,
            None,
        );
        mk(
            "0000:01:00.0",
            "0x10de",
            "0x2420",
            "0x030000",
            Some("nvidia"),
            13,
            Some("0"),
        );
        mk(
            "0000:01:00.1",
            "0x10de",
            "0x228b",
            "0x040300",
            Some("snd_hda_intel"),
            13,
            None,
        );
        if let Some(extra) = dgpu_group_extra {
            mk(
                extra,
                "0x8086",
                "0x51ed",
                "0x0c0330",
                Some("xhci_hcd"),
                13,
                None,
            );
        }
        Sysfs {
            root: dir.to_path_buf(),
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("bcontain-gpu-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn finds_the_free_nvidia_gpu() {
        let sys = laptop_sysfs(&tmp("ok"), None);
        assert!(sys.iommu_enabled());
        let gpus = sys.gpus();
        assert_eq!(gpus.len(), 2);
        let g = sys.passthrough_candidate().unwrap();
        assert_eq!(g.address, "0000:01:00.0");
        assert!(!g.boot_vga);
        assert_eq!(g.iommu_group, Some(13));
        assert_eq!(g.driver.as_deref(), Some("nvidia"));
        // The root port stays with the host; GPU + audio go to the VM.
        assert_eq!(
            passthrough_functions(&g),
            vec!["0000:01:00.0", "0000:01:00.1"]
        );
        let checks = readiness(&sys, Some(&g));
        let group = checks.iter().find(|c| c.item == "IOMMU group").unwrap();
        assert!(group.ok, "{group:?}");
        assert!(checks.iter().find(|c| c.item == "GPU").unwrap().ok);
    }

    #[test]
    fn flags_a_shared_group() {
        let sys = laptop_sysfs(&tmp("shared"), Some("0000:00:14.0"));
        let g = sys.passthrough_candidate().unwrap();
        let checks = readiness(&sys, Some(&g));
        let group = checks.iter().find(|c| c.item == "IOMMU group").unwrap();
        assert!(!group.ok);
        assert!(group.fix.as_ref().unwrap().contains("0000:00:14.0"));
    }

    #[test]
    fn no_iommu_without_groups() {
        let d = tmp("none");
        fs::create_dir_all(d.join("bus/pci/devices")).unwrap();
        let sys = Sysfs { root: d };
        assert!(!sys.iommu_enabled());
        assert!(sys.passthrough_candidate().is_none());
    }
}
