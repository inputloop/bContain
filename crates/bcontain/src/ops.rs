// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Operations shared by the CLI and the web UI.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::engine::{
    self, Disk, DiskBus, Display, Firmware, GuestOs, KernelBoot, PortForward, VmSpec,
};
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
    /// The ISO installs Windows 10/11 (also detected from the disc itself).
    pub windows: bool,
    /// Put the VM's disks in `<storage>/<name>` (any drive) instead of the
    /// VM's folder in the data directory.
    pub storage: Option<PathBuf>,
}

/// What `image` resolved to.
pub enum Resolved {
    Disk(PathBuf, DiskFormat),
    Oci(PathBuf),
}

/// Turn a path or registry reference into something bootable, pulling if needed.
/// Something written as a file path rather than a registry reference: a
/// typo in it should say "not found", not start a Docker Hub pull.
pub fn looks_like_path(s: &str) -> bool {
    let b = s.as_bytes();
    let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    let ext = Path::new(s)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| crate::desktop::IMAGE_EXTS.contains(&e.to_lowercase().as_str()));
    s.starts_with(['/', '.', '~']) || s.contains('\\') || drive || ext
}

pub fn resolve_image(image: &str, log: &mut dyn FnMut(String)) -> Result<Resolved> {
    let p = Path::new(image);
    if !p.exists() && looks_like_path(image) {
        bail!("{image}: file not found");
    }
    if p.exists() {
        return Ok(match image::sniff(p)? {
            ImageKind::Disk(f) => Resolved::Disk(fs::canonicalize(p)?, f),
            ImageKind::OciArchive | ImageKind::OciLayout => Resolved::Oci(fs::canonicalize(p)?),
            ImageKind::Bzip2 => {
                let out = decompress_bzip2(p, log)?;
                match image::sniff(&out)? {
                    ImageKind::Disk(f) => Resolved::Disk(out, f),
                    _ => bail!("{} does not contain a disk image", p.display()),
                }
            }
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

/// Unpack `src` (bzip2) once into the images folder and return the result.
/// Reused on later runs; a partial file never takes the final name.
fn decompress_bzip2(src: &Path, log: &mut dyn FnMut(String)) -> Result<PathBuf> {
    let name = src
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("image.bz2");
    let stem = name.strip_suffix(".bz2").unwrap_or(name);
    let dir = store::images_dir().join("unpacked");
    let out = dir.join(stem);
    let src_len = fs::metadata(src)?.len();
    let stamp = dir.join(format!("{stem}.from"));
    let origin = format!("{}:{src_len}", fs::canonicalize(src)?.display());
    if out.is_file() && fs::read_to_string(&stamp).is_ok_and(|s| s == origin) {
        log(format!("using {} unpacked earlier", out.display()));
        return Ok(out);
    }
    fs::create_dir_all(&dir)?;
    log(format!(
        "unpacking {name} ({} MiB, takes a few minutes)",
        src_len >> 20
    ));
    let partial = dir.join(format!("{stem}.partial"));
    let input = std::io::BufReader::new(fs::File::open(src)?);
    let counted = CountingReader {
        inner: input,
        read: 0,
    };
    let mut decoder = bzip2::read::MultiBzDecoder::new(counted);
    let mut output = std::io::BufWriter::new(fs::File::create(&partial)?);
    let mut buf = vec![0u8; 1 << 20];
    let mut written: u64 = 0;
    let mut next_report = 1u64 << 30;
    loop {
        let n = std::io::Read::read(&mut decoder, &mut buf)
            .with_context(|| format!("decompressing {name}"))?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut output, &buf[..n])?;
        written += n as u64;
        if written >= next_report {
            let pct = decoder.get_ref().read * 100 / src_len.max(1);
            log(format!("  {} GiB unpacked ({pct}%)", written >> 30));
            next_report += 1 << 30;
        }
    }
    std::io::Write::flush(&mut output)?;
    drop(output);
    fs::rename(&partial, &out)?;
    fs::write(&stamp, origin)?;
    log(format!(
        "  unpacked to {} ({} MiB)",
        out.display(),
        written >> 20
    ));
    Ok(out)
}

struct CountingReader<R> {
    inner: R,
    read: u64,
}

impl<R: std::io::Read> std::io::Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        Ok(n)
    }
}

/// SteamOS's recovery image (`steamdeck-repair-*.img[.bz2]`): a USB-stick
/// image whose "Re-image Steam Deck" installer writes to an NVMe drive.
pub fn is_steamos_image(path: &Path) -> bool {
    let n = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let named = n.contains("steamos")
        || (n.contains("steamdeck")
            && ["repair", "recovery", "oobe"].iter().any(|k| n.contains(k)));
    // A renamed file is still SteamOS if its partitions say so (compressed
    // images are recognised by name until they are unpacked).
    named || image::gpt_info(path).is_some_and(|g| g.looks_like_steamos())
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
    let disks_dir = match &opts.storage {
        Some(parent) if persist => {
            if !parent.is_absolute() {
                bail!("the storage folder must be a full path, e.g. D:\\VMs");
            }
            let d = parent.join(&opts.name);
            if d.exists() && fs::read_dir(&d)?.next().is_some() {
                bail!("{} already exists and is not empty", d.display());
            }
            let made_here = !d.exists();
            fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
            (d, made_here)
        }
        _ => (dir.clone(), false),
    };
    let (disks_dir, made_here) = disks_dir;
    let custom_storage = disks_dir != dir;
    // On failure, remove the folder only if bContain made it.
    let mut storage_cleanup = DirGuard(made_here.then(|| disks_dir.clone()));

    let mut spec = VmSpec {
        name: opts.name.clone(),
        cpus: opts.cpus.unwrap_or(sizing.cpus),
        memory_mib: opts.memory_mib.unwrap_or(sizing.memory_mib),
        disks: Vec::new(),
        kernel: None,
        display: opts.display.unwrap_or(sizing.display),
        firmware: opts.firmware.unwrap_or(Firmware::Bios),
        ports: opts.ports.clone(),
        hugepages: false, // decided below, once the memory size is final
        no_reboot: is_container,
        legacy_devices: opts.legacy_devices,
        accel_mode: opts.accel_mode,
        gpu: None,
        guest: GuestOs::Other,
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
        Resolved::Disk(path, format) if is_steamos_image(Path::new(&opts.image)) => {
            // SteamOS installs like on a Steam Deck: boot the recovery image
            // as a USB stick, run "Re-image Steam Deck", which writes to the
            // NVMe drive. The recovery image itself is never modified.
            log("SteamOS recovery image: installer on USB, target on NVMe, UEFI".into());
            let size = opts.disk_mib.filter(|&s| s > 0).unwrap_or(if opts.gpu {
                256 * 1024
            } else {
                128 * 1024
            });
            let disk = disks_dir.join("steamos-nvme.qcow2");
            report_space(&disks_dir, size, log);
            log(format!("creating {} GiB NVMe disk", size / 1024));
            qemu_img_create(&disk, size)?;
            spec.disks.push(Disk {
                path: disk,
                format: DiskFormat::Qcow2,
                cdrom: false,
                ephemeral: false,
                bus: DiskBus::Nvme,
                installer: false,
            });
            spec.disks.push(Disk {
                path,
                format,
                cdrom: false,
                ephemeral: true,
                bus: DiskBus::Usb,
                installer: true,
            });
            spec.firmware = Firmware::Uefi;
            log("  in the installer, double-click \"Re-image Steam Deck\"; then eject the installer".into());
            if !opts.gpu {
                log("  without a passed-through GPU, use Desktop Mode: Gaming Mode needs a Vulkan GPU".into());
            }
        }
        Resolved::Disk(path, DiskFormat::Iso)
            if opts.windows || image::is_windows_installer(&path) =>
        {
            // Windows 11 is a UEFI operating system; recent builds no longer
            // start under SeaBIOS (they stop at the Windows logo).
            log("Windows installer: UEFI, NVMe system disk, SATA CD, e1000e network".into());
            spec.guest = GuestOs::Windows;
            spec.firmware = opts.firmware.unwrap_or(Firmware::Uefi);
            spec.legacy_devices = true; // e1000e: Windows has the driver built in
            if opts.cpus.is_none() {
                spec.cpus = spec.cpus.max(4).min(host.threads as u32);
            }
            if opts.memory_mib.is_none() {
                spec.memory_mib = spec.memory_mib.max(8 * 1024);
            }
            let size = opts.disk_mib.filter(|&s| s > 0).unwrap_or(128 * 1024);
            let disk = disks_dir.join("windows.qcow2");
            report_space(&disks_dir, size, log);
            log(format!("creating {} GiB NVMe system disk", size / 1024));
            qemu_img_create(&disk, size)?;
            spec.disks.push(Disk {
                path: disk,
                format: DiskFormat::Qcow2,
                cdrom: false,
                ephemeral: false,
                bus: DiskBus::Nvme,
                installer: false,
            });
            spec.disks.push(Disk {
                path,
                format: DiskFormat::Iso,
                cdrom: true,
                ephemeral: false,
                bus: DiskBus::Auto,
                installer: true,
            });
            let answers = crate::windows::answer_iso(&disks_dir)?;
            spec.disks.push(Disk {
                path: answers,
                format: DiskFormat::Iso,
                cdrom: true,
                ephemeral: false,
                bus: DiskBus::Auto,
                installer: false,
            });
            log("  answer disc added: Setup skips Windows 11's TPM, Secure Boot, CPU and RAM checks".into());
            log("  \"Press any key to boot from CD\" is pressed for you on the first start".into());
            log("  after installing, stop the VM and use Eject installer".into());
        }
        Resolved::Disk(path, format) => {
            let cdrom = format == DiskFormat::Iso;
            // A GPT disk with an EFI system partition but no BIOS boot
            // partition cannot start under SeaBIOS ("Booting from Hard
            // Disk..." and nothing more).
            if format == DiskFormat::Raw
                && opts.firmware.is_none()
                && image::gpt_info(&path).is_some_and(|g| g.uefi_only())
            {
                log(
                    "UEFI-only disk (GPT with an EFI system partition): using UEFI firmware".into(),
                );
                spec.firmware = Firmware::Uefi;
            }
            if cdrom
                && opts.firmware.is_none()
                && image::iso_boot(&path).is_some_and(|b| b.uefi_only())
            {
                log(
                    "UEFI-only disc (its boot catalog has only an EFI entry): using UEFI firmware"
                        .into(),
                );
                spec.firmware = Firmware::Uefi;
            }
            spec.disks.push(Disk {
                path,
                format,
                cdrom,
                ephemeral: opts.ephemeral && !cdrom,
                bus: DiskBus::Auto,
                installer: false,
            });
            if cdrom {
                let size = opts.disk_mib.unwrap_or(if persist { 64 * 1024 } else { 0 });
                if size > 0 {
                    let disk = disks_dir.join("disk.qcow2");
                    report_space(&disks_dir, size, log);
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
                            bus: DiskBus::Auto,
                            installer: false,
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

    // Hugepages only when the reserved pool covers the final size: the
    // Windows and container branches above may have raised it.
    spec.hugepages = sizing.hugepages && spec.memory_mib <= host.hugepages_free_mib;
    let rec = VmRecord {
        spec,
        preset: preset.name().into(),
        backend: engine,
        created: store::now(),
        source: opts.image.clone(),
        storage: custom_storage.then_some(disks_dir),
    };
    if persist {
        store::save(&rec)?;
    }
    cleanup.0 = None;
    storage_cleanup.0 = None;
    Ok(rec)
}

/// Say where a new disk goes and how much room that drive has. Disks are
/// thin (qcow2): they start small and grow up to `size_mib` as the guest
/// writes, so a tight drive is a warning rather than an error.
fn report_space(dir: &Path, size_mib: u64, log: &mut dyn FnMut(String)) {
    log(format!("disk folder: {}", dir.display()));
    if let Some((free, total, drive)) = store::free_space(dir) {
        let gib = |b: u64| b as f64 / (1u64 << 30) as f64;
        log(format!(
            "  {} has {:.0} GiB free of {:.0} GiB",
            drive.display(),
            gib(free),
            gib(total)
        ));
        if free / (1 << 20) < size_mib {
            log(format!(
                "  warning: the disk can grow to {} GiB, more than is free there",
                size_mib / 1024
            ));
        }
    }
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
    let (r, keys) = start_inner(name)?;
    if let Some(k) = keys {
        std::thread::spawn(move || k.press());
    }
    Ok(r)
}

/// [`start`] for the CLI, which exits as soon as it returns: waits until the
/// boot prompt has been answered.
pub fn start_and_wait(name: &str) -> Result<engine::Running> {
    let (r, keys) = start_inner(name)?;
    if let Some(k) = keys {
        k.press();
    }
    Ok(r)
}

/// Windows install media asks "Press any key to boot from CD or DVD" and,
/// left alone, falls through to the empty disk. Until Windows is installed,
/// press Space for the person while the prompt can be up.
pub struct BootKeys {
    backend: String,
    running: engine::Running,
    seconds: u64,
}

impl BootKeys {
    fn for_vm(rec: &store::VmRecord, running: &engine::Running) -> Option<BootKeys> {
        let spec = &rec.spec;
        let installing = spec.guest == GuestOs::Windows
            && spec.disks.iter().any(|d| d.cdrom && d.installer)
            && spec
                .disks
                .iter()
                .find(|d| !d.cdrom)
                .is_some_and(|d| crate::windows::disk_is_fresh(&d.path));
        installing.then(|| BootKeys {
            backend: rec.backend.clone(),
            running: running.clone(),
            // Setup's first screen, where Space would press a button, takes
            // far longer than this to appear; in software mode much longer.
            seconds: if spec.accel_mode == crate::engine::AccelMode::Software {
                40
            } else {
                15
            },
        })
    }

    pub fn press(self) {
        let Ok(hv) = engine::backend(&self.backend) else {
            return;
        };
        let end = std::time::Instant::now() + std::time::Duration::from_secs(self.seconds);
        std::thread::sleep(std::time::Duration::from_millis(800));
        while std::time::Instant::now() < end && hv.is_alive(&self.running) {
            let _ = hv.press_key(&self.running, "spc");
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
    }
}

fn start_inner(name: &str) -> Result<(engine::Running, Option<BootKeys>)> {
    let mut rec = store::load(name)?;
    if crate::diagnose::busy(name) {
        bail!("`{name}` is being diagnosed; wait for that to finish");
    }
    // Machines made before discs were checked for UEFI-only boot catalogs
    // (Bazzite, for one) sit at SeaBIOS's "code 0009"; switch them over.
    if rec.spec.firmware == Firmware::Bios
        && rec
            .spec
            .disks
            .iter()
            .any(|d| d.cdrom && image::iso_boot(&d.path).is_some_and(|b| b.uefi_only()))
    {
        rec.spec.firmware = Firmware::Uefi;
        store::save(&rec)?;
    }
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
    let keys = BootKeys::for_vm(&rec, &r);
    Ok((r, keys))
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
    // A container's init has no ACPI handler: the power button would do
    // nothing, so containers are always ended outright.
    let force = force || rec.spec.kernel.is_some();
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
    if crate::diagnose::busy(name) {
        bail!("`{name}` is being diagnosed; wait for that to finish");
    }
    let hv = engine::backend(&rec.backend)?;
    if store::running(name, hv.as_ref()).is_some() {
        if !force {
            bail!("`{name}` is running; stop it first or pass --force");
        }
        stop(name, true)?; // waits for QEMU and gives a GPU back
    } else if rec.spec.gpu.is_some() {
        // A crash can leave the GPU on vfio-pci; the record is the last
        // place that says which one to give back.
        let _ = gpu_release(&rec.spec);
    }
    // Only what bContain created is deleted; the source image is left alone.
    if let Some(dir) = &rec.storage {
        for f in store::STORAGE_FILES {
            let p = dir.join(f);
            if p.exists() {
                fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
            }
        }
        let _ = fs::remove_dir(dir); // only if now empty
    }
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
    /// An ISO or installer disk is still attached (offer "Eject installer").
    pub installer: bool,
    /// Where the VM's disks live.
    pub storage: PathBuf,
    /// A diagnosis is booting it in the background (nothing else may).
    pub diagnosing: bool,
    /// Has a passed-through GPU (cannot be diagnosed or switched off Auto).
    pub gpu: bool,
}

pub fn views() -> Result<Vec<VmView>> {
    Ok(store::list()?
        .into_iter()
        .map(|r| {
            // Each VM's own backend decides whether it is alive: asking QEMU
            // about a native VM would report it stopped and drop its run file.
            let state = engine::backend(&r.backend)
                .ok()
                .map_or(store::State::Stopped, |h| {
                    store::state(&r.spec.name, h.as_ref())
                });
            (r, state)
        })
        .map(|(r, state)| VmView {
            storage: r
                .storage
                .clone()
                .unwrap_or_else(|| store::vm_dir(&r.spec.name)),
            accel_mode: r.spec.accel_mode,
            installer: r.spec.disks.iter().any(|d| d.cdrom || d.installer),
            diagnosing: crate::diagnose::busy(&r.spec.name),
            gpu: r.spec.gpu.is_some(),
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

/// Remove a stopped VM's install media (ISO or installer disk), so it boots
/// the installed system. The media files themselves are left alone.
pub fn eject(name: &str) -> Result<usize> {
    let mut rec = store::load(name)?;
    let hv = engine::backend(&rec.backend)?;
    if crate::diagnose::busy(name) {
        bail!("`{name}` is being diagnosed; wait for that to finish");
    }
    if store::running(name, hv.as_ref()).is_some() {
        bail!("stop `{name}` before ejecting its installer");
    }
    let before = rec.spec.disks.len();
    rec.spec.disks.retain(|d| !d.cdrom && !d.installer);
    let removed = before - rec.spec.disks.len();
    if removed == 0 {
        bail!("`{name}` has no installer attached");
    }
    store::save(&rec)?;
    Ok(removed)
}

/// Change how a stopped VM uses the hardware hypervisor.
pub fn set_accel_mode(name: &str, mode: crate::engine::AccelMode) -> Result<()> {
    let mut rec = store::load(name)?;
    let hv = engine::backend(&rec.backend)?;
    if store::running(name, hv.as_ref()).is_some() {
        bail!("stop `{name}` before changing its acceleration");
    }
    if rec.spec.gpu.is_some() && mode != crate::engine::AccelMode::Auto {
        bail!(
            "`{name}` has the GPU: passthrough needs automatic acceleration (KVM, q35, host CPU)"
        );
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
    let create = |options: &[&str]| {
        crate::desktop::helper(
            Command::new(&img)
                .args(["create", "-q", "-f", "qcow2"])
                .args(options)
                .arg(path)
                .arg(format!("{size_mib}M")),
        )
        .output()
        .with_context(|| format!("running {}", img.display()))
    };
    // Bigger clusters with 32 sub-clusters each, and refcounts written
    // lazily: a guest's first writes to fresh space (an OS install) cost
    // less metadata. qemu-img bench, 4 KiB writes into new clusters: 1.8x
    // faster than the defaults; sequential writes unchanged. A qemu-img
    // older than 5.2 lacks extended_l2, so fall back to the defaults.
    let mut out = create(&["-o", "cluster_size=128k,extended_l2=on,lazy_refcounts=on"])?;
    if !out.status.success() {
        let _ = fs::remove_file(path);
        out = create(&[])?;
    }
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
    fn steamos_names() {
        assert!(is_steamos_image(Path::new(
            "/d/steamdeck-repair-20250521.10-3.7.7.img.bz2"
        )));
        assert!(is_steamos_image(Path::new(
            "C:\\x\\steamdeck-recovery-4.img"
        )));
        assert!(!is_steamos_image(Path::new("/d/bazzite-deck-nvidia.iso")));
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
