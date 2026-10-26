// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Operations shared by the CLI and the web UI.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::engine::{self, Disk, Display, Firmware, KernelBoot, PortForward, VmSpec};
use crate::host::Host;
use crate::image::{self, DiskFormat, ImageKind};
use crate::oci;
use crate::preset::Preset;
use crate::registry::{self, Reference};
use crate::store::{self, VmRecord};

#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    pub name: String,
    /// Path to a disk image / ISO / OCI archive / OCI layout, or a registry reference.
    pub image: String,
    pub preset: Option<String>,
    pub cpus: Option<u32>,
    pub memory_mib: Option<u64>,
    /// Size of the blank disk created alongside an ISO, in MiB. `Some(0)` = none.
    pub disk_mib: Option<u64>,
    pub display: Option<Display>,
    pub firmware: Option<Firmware>,
    pub legacy_devices: bool,
    pub ephemeral: bool,
    pub ports: Vec<PortForward>,
    /// Override the container command (OCI images only).
    pub command: Vec<String>,
    pub env: Vec<String>,
    pub kernel: Option<PathBuf>,
    /// "qemu" (default) or "native" (bContain's own hypervisor, containers only).
    pub engine: Option<String>,
    pub accel_mode: crate::engine::AccelMode,
    /// Hand the host's free GPU to this VM (Linux + KVM).
    pub gpu: bool,
}

/// What `image` resolved to.
pub enum Resolved {
    Disk(PathBuf, DiskFormat),
    Oci(PathBuf),
}

/// Turn a path or registry reference into something bootable, pulling if needed.
pub fn resolve_image(image: &str, log: &mut dyn FnMut(String)) -> Result<Resolved> {
    let p = Path::new(image);
    if p.exists() {
        return Ok(match image::sniff(p)? {
            ImageKind::Disk(f) => Resolved::Disk(fs::canonicalize(p)?, f),
            ImageKind::OciArchive | ImageKind::OciLayout => Resolved::Oci(fs::canonicalize(p)?),
        });
    }
    let r = Reference::parse(image)
        .with_context(|| format!("`{image}` is not a file and not an image reference"))?;
    let dir = store::images_dir().join(image_dir_name(&r));
    if !dir.join("index.json").is_file() {
        log(format!("pulling {}", r.display()));
        registry::Client::new().pull(&r, &dir, |d, i, n| {
            log(format!("  [{i}/{n}] {}", &d[..d.len().min(19)]));
        })?;
    }
    Ok(Resolved::Oci(dir))
}

pub fn image_dir_name(r: &Reference) -> String {
    let repo = r
        .repository
        .strip_prefix("library/")
        .unwrap_or(&r.repository);
    let mut s = format!("{repo}_{}", r.reference.trim_start_matches("sha256:"));
    if r.registry != "registry-1.docker.io" {
        s = format!("{}_{s}", r.registry);
    }
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Build the spec for `opts` and, for persistent VMs, save it.
pub fn create(
    opts: &CreateOptions,
    host: &Host,
    persist: bool,
    log: &mut dyn FnMut(String),
) -> Result<VmRecord> {
    store::validate_name(&opts.name)?;
    if persist && store::exists(&opts.name) {
        bail!("a VM named `{}` already exists", opts.name);
    }
    let resolved = resolve_image(&opts.image, log)?;
    let is_container = matches!(resolved, Resolved::Oci(_));
    let preset = match &opts.preset {
        Some(p) => Preset::parse(p)?,
        None if is_container => Preset::Container,
        None if opts.gpu => Preset::Gaming,
        None => Preset::Rtx3080ti,
    };
    let sizing = preset.sizing(host);
    let dir = store::vm_dir(&opts.name);
    fs::create_dir_all(&dir)?;
    // Remove the half-made folder (disk, initramfs) if anything below fails.
    let mut cleanup = DirGuard(Some(dir.clone()));

    let mut spec = VmSpec {
        name: opts.name.clone(),
        cpus: opts.cpus.unwrap_or(sizing.cpus),
        memory_mib: opts.memory_mib.unwrap_or(sizing.memory_mib),
        disks: Vec::new(),
        kernel: None,
        display: opts.display.unwrap_or(sizing.display),
        firmware: opts.firmware.unwrap_or(Firmware::Bios),
        ports: opts.ports.clone(),
        hugepages: sizing.hugepages && opts.memory_mib.is_none_or(|m| m <= host.hugepages_free_mib),
        no_reboot: is_container,
        legacy_devices: opts.legacy_devices,
        accel_mode: opts.accel_mode,
        gpu: None,
    };
    if opts.gpu {
        spec.gpu = Some(gpu_for_new_vm(&dir, log)?);
        spec.firmware = Firmware::Uefi;
        spec.accel_mode = crate::engine::AccelMode::Auto;
        for (port, udp) in SUNSHINE_PORTS {
            if !spec.ports.iter().any(|p| p.host == port && p.udp == udp) {
                spec.ports.push(PortForward {
                    host: port,
                    guest: port,
                    udp,
                });
            }
        }
        log("  forwarding Sunshine's ports: connect Moonlight to 127.0.0.1".into());
    }
    if spec.cpus as usize > host.threads {
        bail!(
            "{} vCPUs requested but the host has {} threads",
            spec.cpus,
            host.threads
        );
    }

    match resolved {
        Resolved::Disk(path, format) => {
            let cdrom = format == DiskFormat::Iso;
            spec.disks.push(Disk {
                path,
                format,
                cdrom,
                ephemeral: opts.ephemeral && !cdrom,
            });
            if cdrom {
                let size = opts.disk_mib.unwrap_or(if persist { 64 * 1024 } else { 0 });
                if size > 0 {
                    let disk = dir.join("disk.qcow2");
                    log(format!(
                        "creating {} GiB disk {}",
                        size / 1024,
                        disk.display()
                    ));
                    qemu_img_create(&disk, size)?;
                    spec.disks.insert(
                        0,
                        Disk {
                            path: disk,
                            format: DiskFormat::Qcow2,
                            cdrom: false,
                            ephemeral: false,
                        },
                    );
                }
            }
        }
        Resolved::Oci(path) => {
            let img = oci::Image::open(&path)?;
            if !img.tags.is_empty() {
                log(format!("image {}", img.tags.join(", ")));
            }
            if let Some(user) = img
                .config
                .user
                .as_deref()
                .filter(|u| !u.is_empty() && *u != "root" && *u != "0")
            {
                log(format!("  note: image USER `{user}` is ignored; the container runs as root inside its VM"));
            }
            if let Some(exposed) = &img.config.exposed_ports {
                if opts.ports.is_empty() && !exposed.is_empty() {
                    let list: Vec<&str> = exposed
                        .keys()
                        .map(|k| k.split('/').next().unwrap_or(k))
                        .collect();
                    log(format!(
                        "  image exposes {}; forward with -p HOST:GUEST",
                        list.join(", ")
                    ));
                }
            }
            let argv = img.argv(&opts.command);
            if argv.is_empty() {
                bail!("image has no Entrypoint/Cmd; pass a command after `--`");
            }
            let mut env = img.config.env.clone().unwrap_or_default();
            env.extend(opts.env.iter().cloned());
            let run = oci::RunOptions {
                argv,
                env,
                workdir: img
                    .config
                    .working_dir
                    .clone()
                    .filter(|w| !w.is_empty())
                    .unwrap_or_else(|| "/".into()),
                hostname: opts.name.clone(),
            };
            let initrd = dir.join("initramfs.cpio.gz");
            log(format!("building initramfs from {}", path.display()));
            let bytes = oci::build_initramfs(&img, &run, &initrd)?;
            log(format!("  rootfs {:.1} MiB", bytes as f64 / 1048576.0));
            // An initramfs lives in guest RAM: make sure it fits with room to run.
            let need = bytes / 1048576 * 2 + 512;
            if spec.memory_mib < need {
                log(format!("  raising memory to {need} MiB to hold the rootfs"));
                spec.memory_mib = need;
            }
            spec.kernel = Some(KernelBoot {
                kernel: oci::find_kernel(opts.kernel.as_deref())?,
                initrd: Some(initrd),
                cmdline: oci::kernel_cmdline(),
            });
            spec.display = Display::Headless;
        }
    }

    let engine = opts.engine.clone().unwrap_or_else(|| "qemu".into());
    match engine.as_str() {
        "qemu" => {}
        "native" => {
            if spec.kernel.is_none() {
                bail!(
                    "bContain's own hypervisor runs container images only for now; \
                     use the qemu engine for disk images and ISOs"
                );
            }
            if spec.cpus > 1 {
                log("  bContain's own hypervisor runs 1 vCPU for now".into());
                spec.cpus = 1;
            }
            if spec.memory_mib > crate::native::MAX_MEMORY_MIB {
                log(format!(
                    "  memory capped at {} MiB on bContain's own hypervisor",
                    crate::native::MAX_MEMORY_MIB
                ));
                spec.memory_mib = crate::native::MAX_MEMORY_MIB;
            }
            if !spec.ports.is_empty() {
                log("  port forwarding is ignored: bContain's own hypervisor has no network card yet".into());
                spec.ports.clear();
            }
        }
        other => bail!("unknown engine `{other}` (expected `qemu` or `native`)"),
    }

    let rec = VmRecord {
        spec,
        preset: preset.name().into(),
        backend: engine,
        created: store::now(),
        source: opts.image.clone(),
    };
    if persist {
        store::save(&rec)?;
    }
    cleanup.0 = None;
    Ok(rec)
}

struct DirGuard(Option<PathBuf>);

impl Drop for DirGuard {
    fn drop(&mut self) {
        if let Some(d) = self.0.take() {
            let _ = fs::remove_dir_all(d);
        }
    }
}

pub fn start(name: &str) -> Result<engine::Running> {
    let rec = store::load(name)?;
    let hv = engine::backend(&rec.backend)?;
    hv.check()
        .with_context(|| format!("{} backend is not usable", hv.name()))?;
    if store::running(name, hv.as_ref()).is_some() {
        bail!("`{name}` is already running");
    }
    gpu_acquire(&rec.spec)?;
    let r = match hv.launch(&rec.spec, &store::log_path(name)) {
        Ok(r) => r,
        Err(e) => {
            let _ = gpu_release(&rec.spec);
            return Err(e);
        }
    };
    store::set_running(name, &r)?;
    Ok(r)
}

/// Take the VM's GPU away from the host (bind it to vfio-pci).
pub fn gpu_acquire(spec: &VmSpec) -> Result<()> {
    if let Some(g) = &spec.gpu {
        crate::gpu::bind_vfio(&crate::gpu::Sysfs::default(), &g.functions)
            .context("handing the GPU to the VM")?;
    }
    Ok(())
}

/// Give the VM's GPU back to the host's drivers.
pub fn gpu_release(spec: &VmSpec) -> Result<()> {
    if let Some(g) = &spec.gpu {
        crate::gpu::unbind_vfio(&crate::gpu::Sysfs::default(), &g.functions)
            .context("giving the GPU back to the host")?;
    }
    Ok(())
}

/// Check the host can pass its GPU through and describe it for a new VM.
fn gpu_for_new_vm(
    dir: &Path,
    log: &mut dyn FnMut(String),
) -> Result<crate::engine::GpuPassthrough> {
    let sys = crate::gpu::Sysfs::default();
    let candidate = sys.passthrough_candidate();
    let blockers: Vec<String> = crate::gpu::readiness(&sys, candidate.as_ref())
        .into_iter()
        .filter(|c| !c.ok)
        .map(|c| format!("  - {}: {}", c.item, c.fix.unwrap_or(c.detail)))
        .collect();
    if !blockers.is_empty() {
        bail!(
            "this host cannot pass a GPU through yet:\n{}",
            blockers.join("\n")
        );
    }
    let g = candidate.expect("readiness passed, so there is a GPU");
    log(format!("  passing through {} ({})", g.name, g.address));
    // Laptop GPUs often keep their video BIOS in system firmware; save it if
    // the card exposes it (needs root), otherwise run without one.
    let rom = dir.join("vbios.rom");
    let rom = match crate::gpu::dump_vbios(&sys, &g.address, &rom) {
        Ok(n) => {
            log(format!("  saved the GPU's video BIOS ({} KiB)", n / 1024));
            Some(rom)
        }
        Err(e) => {
            log(format!(
                "  no video BIOS image ({e:#}); continuing without one"
            ));
            None
        }
    };
    Ok(crate::engine::GpuPassthrough {
        functions: crate::gpu::passthrough_functions(&g),
        subsystem_vendor: g.subsystem_vendor,
        subsystem_device: g.subsystem_device,
        rom,
    })
}

pub fn stop(name: &str, force: bool) -> Result<()> {
    let rec = store::load(name)?;
    let hv = engine::backend(&rec.backend)?;
    let r =
        store::running(name, hv.as_ref()).with_context(|| format!("`{name}` is not running"))?;
    hv.stop(&r, force)?;
    if force {
        store::clear_running(name);
    }
    if rec.spec.gpu.is_some() {
        // The GPU can only go back once QEMU has let go of it.
        for _ in 0..600 {
            if !hv.is_alive(&r) {
                store::clear_running(name);
                return gpu_release(&rec.spec);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        bail!(
            "`{name}` is still shutting down; once it has stopped, give the GPU back with \
             `sudo bcontain gpu release {name}`"
        );
    }
    Ok(())
}

pub fn remove(name: &str, force: bool) -> Result<()> {
    let rec = store::load(name)?;
    let hv = engine::backend(&rec.backend)?;
    if let Some(r) = store::running(name, hv.as_ref()) {
        if !force {
            bail!("`{name}` is running; stop it first or pass --force");
        }
        hv.stop(&r, true)?;
    }
    // Only bContain's own folder is deleted; the source image is left alone.
    store::remove(name)
}

#[derive(Serialize)]
pub struct VmView {
    pub name: String,
    pub state: store::State,
    pub preset: String,
    pub cpus: u32,
    pub memory_mib: u64,
    pub display: Display,
    pub kind: &'static str,
    pub source: String,
    pub ports: Vec<PortForward>,
    /// For a stopped VM whose backend log ends in an error: that error, so
    /// the app can say why it stopped instead of it just vanishing.
    pub last_error: Option<String>,
    pub accel_mode: crate::engine::AccelMode,
}

pub fn views() -> Result<Vec<VmView>> {
    let hv = engine::backend("qemu").ok();
    Ok(store::list()?
        .into_iter()
        .map(|r| {
            let state = hv.as_ref().map_or(store::State::Stopped, |h| {
                store::state(&r.spec.name, h.as_ref())
            });
            (r, state)
        })
        .map(|(r, state)| VmView {
            accel_mode: r.spec.accel_mode,
            last_error: (state == store::State::Stopped)
                .then(|| last_error(&store::log_path(&r.spec.name)))
                .flatten(),
            state,
            name: r.spec.name.clone(),
            preset: r.preset,
            cpus: r.spec.cpus,
            memory_mib: r.spec.memory_mib,
            display: r.spec.display,
            kind: if r.spec.kernel.is_some() {
                "container"
            } else {
                "vm"
            },
            source: r.source,
            ports: r.spec.ports,
        })
        .collect())
}

/// Change how a stopped VM uses the hardware hypervisor.
pub fn set_accel_mode(name: &str, mode: crate::engine::AccelMode) -> Result<()> {
    let mut rec = store::load(name)?;
    let hv = engine::backend(&rec.backend)?;
    if store::running(name, hv.as_ref()).is_some() {
        bail!("stop `{name}` before changing its acceleration");
    }
    rec.spec.accel_mode = mode;
    store::save(&rec)
}

/// The error a crashed backend left at the end of its log, if any.
fn last_error(log: &Path) -> Option<String> {
    let text = fs::read_to_string(log).ok()?;
    let tail: Vec<&str> = text.lines().rev().take(12).collect();
    tail.into_iter()
        .rev()
        .filter(|l| {
            let l = l.to_lowercase();
            l.contains("qemu-system")
                || l.contains("whpx")
                || l.contains("error")
                || l.contains("abort")
                || l.contains("assertion")
                || l.contains("failed")
        })
        .find(|l| !l.starts_with('#'))
        .map(|l| l.trim().chars().take(300).collect())
}

/// Download a container-ready kernel from an OCI image that carries `/kernel`
/// (linuxkit/kernel) and install it as the guest kernel.
pub fn fetch_kernel(image: &str, log: &mut dyn FnMut(String)) -> Result<PathBuf> {
    let Resolved::Oci(dir) = resolve_image(image, log)? else {
        bail!("{image} is not an OCI image");
    };
    let tree = oci::Image::open(&dir)?.flatten()?;
    let Some(oci::Node {
        kind: oci::Kind::File(data),
        ..
    }) = tree.nodes.get("kernel")
    else {
        bail!("{image} has no /kernel file");
    };
    let target = store::kernel_dir().join("vmlinuz");
    fs::create_dir_all(store::kernel_dir())?;
    fs::write(&target, data)?;
    log(format!(
        "installed kernel ({:.1} MiB)",
        data.len() as f64 / 1048576.0
    ));
    Ok(target)
}

fn qemu_img_create(path: &Path, size_mib: u64) -> Result<()> {
    let qemu = crate::qemu::Qemu::locate()?;
    let exe = if cfg!(windows) {
        "qemu-img.exe"
    } else {
        "qemu-img"
    };
    let img = qemu.binary.with_file_name(exe);
    let img = if img.is_file() {
        img
    } else {
        PathBuf::from(exe)
    };
    let out = crate::desktop::helper(
        Command::new(&img)
            .args(["create", "-q", "-f", "qcow2"])
            .arg(path)
            .arg(format!("{size_mib}M")),
    )
    .output()
    .with_context(|| format!("running {}", img.display()))?;
    if !out.status.success() {
        bail!(
            "qemu-img create failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Parse sizes like `16G`, `512M`, `2048` (MiB).
pub fn parse_mib(s: &str) -> Result<u64> {
    let t = s.trim();
    let t = t
        .strip_suffix("iB")
        .or_else(|| t.strip_suffix('B'))
        .unwrap_or(t);
    let (num, mult) = match t.chars().last() {
        Some('G' | 'g') => (&t[..t.len() - 1], 1024.0),
        Some('T' | 't') => (&t[..t.len() - 1], 1024.0 * 1024.0),
        Some('M' | 'm') => (&t[..t.len() - 1], 1.0),
        _ => (t, 1.0),
    };
    let n: f64 = num
        .trim()
        .parse()
        .with_context(|| format!("bad size `{s}`"))?;
    Ok((n * mult) as u64)
}

/// `HOST:GUEST`, `PORT`, with an optional `/udp` (or `/tcp`) suffix.
pub fn parse_port(s: &str) -> Result<PortForward> {
    let (ports, udp) = match s.trim().rsplit_once('/') {
        Some((p, "udp")) => (p, true),
        Some((p, "tcp")) => (p, false),
        Some(_) => bail!("bad port mapping `{s}` (protocol must be tcp or udp)"),
        None => (s.trim(), false),
    };
    let (h, g) = ports.split_once(':').unwrap_or((ports, ports));
    Ok(PortForward {
        host: h
            .parse()
            .with_context(|| format!("bad port mapping `{s}`"))?,
        guest: g
            .parse()
            .with_context(|| format!("bad port mapping `{s}`"))?,
        udp,
    })
}

/// Sunshine (game streaming, built into Bazzite) listens on these. With
/// them forwarded, Moonlight on the host connects to 127.0.0.1 and shows the
/// VM's GPU output, and carries keyboard, mouse and controller input back.
pub const SUNSHINE_PORTS: [(u16, bool); 9] = [
    (47984, false),
    (47989, false),
    (47990, false),
    (48010, false),
    (47998, true),
    (47999, true),
    (48000, true),
    (48002, true),
    (48010, true),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_crash_reason() {
        let p = std::env::temp_dir().join(format!("bcontain-log-{}", std::process::id()));
        fs::write(&p, "# qemu-system-x86_64 -name t\nSeaBIOS\nqemu-system-x86_64: WHPX: Unexpected VP exit code 4\n").unwrap();
        assert_eq!(
            last_error(&p).unwrap(),
            "qemu-system-x86_64: WHPX: Unexpected VP exit code 4"
        );
        fs::write(&p, "# qemu-system-x86_64 -name t\nbooted fine\n").unwrap();
        assert!(last_error(&p).is_none());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_mib("16G").unwrap(), 16384);
        assert_eq!(parse_mib("512M").unwrap(), 512);
        assert_eq!(parse_mib("1.5GiB").unwrap(), 1536);
        assert_eq!(parse_mib("2048").unwrap(), 2048);
        assert!(parse_mib("lots").is_err());
    }

    #[test]
    fn ports() {
        let p = parse_port("8080:80").unwrap();
        assert_eq!((p.host, p.guest), (8080, 80));
        assert_eq!(parse_port("22").unwrap().guest, 22);
        let u = parse_port("47998/udp").unwrap();
        assert!(u.udp && u.host == 47998 && u.guest == 47998);
        assert!(!parse_port("80:8080/tcp").unwrap().udp);
        assert!(parse_port("80/sctp").is_err());
    }

    #[test]
    fn dir_names() {
        let r = Reference::parse("alpine:3.20").unwrap();
        assert_eq!(image_dir_name(&r), "alpine_3.20");
        let r = Reference::parse("ghcr.io/a/b:1").unwrap();
        assert_eq!(image_dir_name(&r), "ghcr.io_a_b_1");
    }
}
