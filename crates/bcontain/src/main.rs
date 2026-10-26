// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! bcontain: a standalone VM + container runner for Windows and Linux,
//! tuned for RTX 3080 Ti / Core i9 / 32 GB laptops (or better).

mod desktop;
mod diagnose;
mod engine;
mod gpu;
mod host;
mod image;
mod iso;
mod native;
mod oci;
mod ops;
mod preset;
mod qemu;
mod registry;
mod settings;
mod store;
mod usb;
mod web;
mod windows;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::engine::{Display, Firmware, Hypervisor};
use crate::host::Host;
use crate::preset::Preset;

#[derive(Parser)]
#[command(
    name = "bcontain",
    version,
    about = "Standalone VMs and containers, tuned for RTX 3080 Ti / Core i9 / 32 GB"
)]
struct Cli {
    /// With no command, bContain opens its app window (what a double-click does).
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check this host against the reference hardware and report what to fix.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Show the sizing each preset gives on this host.
    Presets,
    /// Define a VM from a disk image, ISO, OCI archive/layout or registry image.
    Create {
        name: String,
        image: String,
        #[command(flatten)]
        tune: Tune,
        /// Size of the new system disk for ISO installers, Windows and SteamOS
        /// (e.g. 64G, 256G, 1T; 0 for none next to a plain ISO).
        #[arg(long)]
        disk_size: Option<String>,
        /// Folder for the VM's disks, on any drive (e.g. D:\VMs). bContain
        /// makes a subfolder named after the VM there.
        #[arg(long)]
        storage: Option<PathBuf>,
        /// Command to run instead of the image's default (containers).
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Boot a VM in the background (or attached with --attach).
    Start {
        name: String,
        /// Stay attached to the serial console (headless VMs).
        #[arg(long)]
        attach: bool,
    },
    /// Ask a VM to shut down (ACPI); --force pulls the plug.
    Stop {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// List VMs.
    #[command(alias = "ls")]
    List,
    /// Show a VM's definition and the exact backend command line.
    Info { name: String },
    /// Detach a VM's installer (ISO or SteamOS recovery image) so it boots the installed system.
    Eject { name: String },
    /// Change a stopped VM's hardware or display settings.
    Set {
        name: String,
        #[arg(long)]
        cpus: Option<u32>,
        /// e.g. 16G.
        #[arg(long)]
        memory: Option<String>,
        /// HOST:GUEST[/udp]; replaces the VM's forwards (repeatable; `--port ''` clears).
        #[arg(short = 'p', long = "port")]
        ports: Option<Vec<String>>,
        /// Open the VM's window full screen.
        #[arg(long)]
        fullscreen: Option<bool>,
        /// Preferred screen resolution, e.g. 1920x1080 (`''` for the guest's choice).
        #[arg(long)]
        resolution: Option<String>,
        /// A TPM 2.0 from swtpm (Linux hosts).
        #[arg(long)]
        tpm: Option<bool>,
        /// UEFI Secure Boot with Microsoft's keys (Linux hosts).
        #[arg(long)]
        secure_boot: Option<bool>,
        /// Share a host folder with the guest (`''` stops sharing). The guest
        /// opens \\10.0.2.4\qemu on Linux hosts, \\10.0.2.2\bcontain-NAME on Windows.
        #[arg(long)]
        share: Option<String>,
        /// A host USB device for the VM, VENDOR:PRODUCT in hex as `bcontain usb`
        /// lists them (repeatable; replaces the list; `--usb ''` clears).
        #[arg(long)]
        usb: Option<Vec<String>>,
    },
    /// List the host's USB devices a VM can take over.
    Usb,
    /// Grow a stopped VM's system disk (never shrinks), e.g. `resize win11 256G`.
    Resize { name: String, size: String },
    /// Snapshots of a stopped VM's disks: list, take, restore or delete.
    Snapshot {
        name: String,
        #[command(subcommand)]
        cmd: Option<SnapshotCmd>,
    },
    /// Print a VM's log (backend output and serial console).
    Logs { name: String },
    /// Delete a VM's definition and the disks bContain created for it.
    #[command(alias = "rm")]
    Remove {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// Run an image once, attached to this terminal, then throw it away.
    Run {
        image: String,
        #[command(flatten)]
        tune: Tune,
        /// Keep writes to the disk image (default: discard them).
        #[arg(long)]
        persist: bool,
        /// Print the backend command line instead of running it.
        #[arg(long)]
        dry_run: bool,
        /// Command to run instead of the image's default (containers).
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Download an image from a registry (Docker Hub by default).
    Pull { image: String },
    /// List downloaded images.
    Images,
    /// Manage the guest kernel used for container micro-VMs.
    Kernel {
        #[command(subcommand)]
        cmd: KernelCmd,
    },
    /// GPU passthrough: check this host, or hand the GPU over and back.
    Gpu {
        #[command(subcommand)]
        cmd: Option<GpuCmd>,
    },
    /// Check that bContain's own hypervisor works on this machine, step by step.
    HypervisorTest,
    /// Find what stops a VM before its firmware draws anything: boots it
    /// several ways without a window and reports which ones get further.
    Diagnose { name: String },
    /// Internal: run a VM on bContain's own hypervisor (used by `start`).
    #[command(name = "__native-run", hide = true)]
    NativeRun { name: String },
    /// Serve the local web dashboard.
    Ui {
        #[arg(long, default_value_t = 7878)]
        port: u16,
        /// Open the dashboard in the default browser.
        #[arg(long)]
        open: bool,
    },
}

#[derive(Subcommand)]
enum GpuCmd {
    /// Show the GPUs and whether one can be passed to a VM (the default).
    Status,
    /// Give a VM's GPU back to the host (if the VM stopped on its own).
    Release { name: String },
    /// Save the GPU's video BIOS to a file (root).
    DumpVbios { out: PathBuf },
    /// Use a video BIOS file for a VM's GPU (laptops whose ROM is not on the card).
    SetRom { name: String, file: PathBuf },
}

#[derive(Subcommand)]
enum KernelCmd {
    /// Download a container-ready kernel (from the linuxkit/kernel image).
    Fetch {
        #[arg(long, default_value = KERNEL_IMAGE)]
        image: String,
    },
    /// Install a local bzImage as the guest kernel.
    Install { path: PathBuf },
    /// Show which kernel will be used.
    Path,
}

pub const KERNEL_IMAGE: &str = "linuxkit/kernel:6.12.59";

#[derive(Args, Default)]
struct Tune {
    /// Sizing preset: rtx3080ti, light, max, container.
    #[arg(long)]
    preset: Option<String>,
    #[arg(long)]
    cpus: Option<u32>,
    /// Guest RAM, e.g. 16G or 4096M.
    #[arg(long)]
    memory: Option<String>,
    #[arg(long, value_enum)]
    display: Option<DisplayArg>,
    #[arg(long, value_enum)]
    firmware: Option<FirmwareArg>,
    /// SATA + e1000e instead of virtio (stock Windows installers).
    #[arg(long)]
    legacy_devices: bool,
    /// The ISO installs Windows 10/11 (usually detected from the disc):
    /// UEFI, NVMe, e1000e, and an answer disc that skips Windows 11's TPM check.
    #[arg(long)]
    windows: bool,
    /// Forward a host port to the guest, HOST:GUEST (repeatable).
    #[arg(short = 'p', long = "port")]
    ports: Vec<String>,
    /// Extra environment for containers, KEY=VALUE (repeatable).
    #[arg(short = 'e', long = "env")]
    env: Vec<String>,
    /// Guest kernel for containers (bzImage).
    #[arg(long)]
    kernel: Option<PathBuf>,
    /// qemu (default), or native: bContain's own hypervisor (containers only, experimental).
    #[arg(long, value_parser = ["qemu", "native"])]
    engine: Option<String>,
    /// auto (default); compat: WHPX with kernel-irqchip=off, for Windows builds
    /// where VMs hang before the BIOS shows anything; software: no hardware
    /// acceleration (slow, but rules the hypervisor out).
    #[arg(long, value_enum, default_value_t = AccelArg::Auto)]
    accel: AccelArg,
    /// Give the host's free GPU to the VM (Linux + KVM; see docs/GPU-PASSTHROUGH.md).
    #[arg(long)]
    gpu: bool,
}

#[derive(Subcommand)]
enum SnapshotCmd {
    /// List the VM's snapshots (the default).
    List,
    /// Save the VM's disks as they are now.
    Take {
        snapshot: String,
    },
    /// Put the VM's disks back to a snapshot; changes since are lost.
    Restore {
        snapshot: String,
    },
    Delete {
        snapshot: String,
    },
}

#[derive(Clone, Copy, ValueEnum, Default)]
enum AccelArg {
    #[default]
    Auto,
    Compat,
    Software,
}

#[derive(Clone, Copy, ValueEnum)]
enum DisplayArg {
    Virgl,
    Basic,
    Headless,
}

#[derive(Clone, Copy, ValueEnum)]
enum FirmwareArg {
    Bios,
    Uefi,
}

impl Tune {
    fn into_options(self, name: String, image: String) -> Result<ops::CreateOptions> {
        Ok(ops::CreateOptions {
            name,
            image,
            preset: self.preset,
            cpus: self.cpus,
            memory_mib: self.memory.as_deref().map(ops::parse_mib).transpose()?,
            disk_mib: None,
            display: self.display.map(|d| match d {
                DisplayArg::Virgl => Display::Virgl,
                DisplayArg::Basic => Display::Basic,
                DisplayArg::Headless => Display::Headless,
            }),
            firmware: self.firmware.map(|f| match f {
                FirmwareArg::Bios => Firmware::Bios,
                FirmwareArg::Uefi => Firmware::Uefi,
            }),
            legacy_devices: self.legacy_devices,
            windows: self.windows,
            storage: None,
            ephemeral: false,
            ports: self
                .ports
                .iter()
                .map(|p| ops::parse_port(p))
                .collect::<Result<_>>()?,
            command: Vec::new(),
            env: self.env,
            kernel: self.kernel,
            engine: self.engine,
            gpu: self.gpu,
            accel_mode: match self.accel {
                AccelArg::Auto => engine::AccelMode::Auto,
                AccelArg::Compat => engine::AccelMode::Compat,
                AccelArg::Software => engine::AccelMode::Software,
            },
        })
    }
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn say(msg: String) {
    eprintln!("{msg}");
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    // No subcommand: started by double-clicking the executable. Open the app.
    let Some(cmd) = cli.cmd else {
        return web::app();
    };
    match cmd {
        Cmd::Doctor { json } => doctor(json),
        Cmd::Presets => {
            let host = Host::detect();
            println!(
                "Presets on this host ({} threads, {:.1} GiB):\n",
                host.threads,
                host.memory_gib()
            );
            println!(
                "  {:<11} {:>5} {:>9}  {:<9} HUGEPAGES",
                "PRESET", "vCPU", "RAM", "DISPLAY"
            );
            for p in Preset::ALL {
                let s = p.sizing(&host);
                println!(
                    "  {:<11} {:>5} {:>7.1}G  {:<9} {}",
                    p.name(),
                    s.cpus,
                    s.memory_mib as f64 / 1024.0,
                    format!("{:?}", s.display).to_lowercase(),
                    if s.hugepages { "yes" } else { "no" }
                );
            }
            Ok(())
        }
        Cmd::Create {
            name,
            image,
            tune,
            disk_size,
            storage,
            command,
        } => {
            let host = Host::detect();
            let mut opts = tune.into_options(name, image)?;
            opts.command = command;
            opts.disk_mib = disk_size.as_deref().map(ops::parse_mib).transpose()?;
            opts.storage = storage;
            let rec = ops::create(&opts, &host, true, &mut say)?;
            println!(
                "created `{}`: {} vCPU, {} MiB, {:?} display (preset {})",
                rec.spec.name, rec.spec.cpus, rec.spec.memory_mib, rec.spec.display, rec.preset
            );
            println!("start it with: bcontain start {}", rec.spec.name);
            Ok(())
        }
        Cmd::Start { name, attach } => {
            if attach {
                let rec = store::load(&name)?;
                let hv = engine::backend(&rec.backend)?;
                if store::running(&name, hv.as_ref()).is_some() {
                    bail!("`{name}` is already running");
                }
                ops::gpu_acquire(&rec.spec)?;
                let code = hv.run_foreground(&rec.spec);
                ops::gpu_release(&rec.spec)?;
                std::process::exit(code?);
            }
            let r = ops::start_and_wait(&name)?;
            println!(
                "started `{name}` (pid {}, QMP 127.0.0.1:{})",
                r.pid, r.control_port
            );
            Ok(())
        }
        Cmd::Stop { name, force } => {
            ops::stop(&name, force)?;
            println!(
                "{} `{name}`",
                if force {
                    "killed"
                } else {
                    "sent ACPI shutdown to"
                }
            );
            Ok(())
        }
        Cmd::List => {
            let vms = ops::views()?;
            if vms.is_empty() {
                println!("no VMs yet; try `bcontain create <name> <image>`");
                return Ok(());
            }
            println!(
                "{:<18} {:<8} {:<10} {:<10} {:>4} {:>8}  SOURCE",
                "NAME", "STATE", "KIND", "PRESET", "vCPU", "RAM"
            );
            for v in vms {
                println!(
                    "{:<18} {:<8} {:<10} {:<10} {:>4} {:>7.1}G  {}",
                    v.name,
                    format!("{:?}", v.state).to_lowercase(),
                    v.kind,
                    v.preset,
                    v.cpus,
                    v.memory_mib as f64 / 1024.0,
                    v.source
                );
            }
            Ok(())
        }
        Cmd::Info { name } => {
            let rec = store::load(&name)?;
            println!("{}", serde_json::to_string_pretty(&rec)?);
            if rec.backend == "native" {
                println!(
                    "\nengine: bContain's own hypervisor ({})",
                    bvmm::probe().accel
                );
                return Ok(());
            }
            let q = qemu::Qemu::locate()?;
            println!(
                "\n{} {}",
                q.binary.display(),
                qemu::shell_join(&q.args(&rec.spec, false)?)
            );
            Ok(())
        }
        Cmd::Set {
            name,
            cpus,
            memory,
            ports,
            fullscreen,
            resolution,
            tpm,
            secure_boot,
            share,
            usb,
        } => {
            let host_usb = usb::list();
            let usb = usb
                .map(|list| {
                    list.iter()
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| {
                            let (v, p) = s.split_once(':').with_context(|| {
                                format!("`{s}`: expected VENDOR:PRODUCT, e.g. 045e:0b12")
                            })?;
                            let (vendor, product) = (
                                u16::from_str_radix(v.trim(), 16)?,
                                u16::from_str_radix(p.trim(), 16)?,
                            );
                            let name = host_usb
                                .iter()
                                .find(|h| h.vendor == vendor && h.product == product)
                                .map(|h| h.name.clone())
                                .unwrap_or_default();
                            Ok(usb::UsbDevice {
                                vendor,
                                product,
                                name,
                            })
                        })
                        .collect::<Result<Vec<_>>>()
                })
                .transpose()?;
            let patch = settings::Patch {
                usb,
                shared_folder: share,
                cpus,
                memory,
                ports,
                fullscreen,
                resolution,
                tpm,
                secure_boot,
            };
            let rec = settings::apply(&name, &Host::detect(), &patch)?;
            println!(
                "`{name}`: {} vCPU, {} MiB, {} port forward(s){}{}",
                rec.spec.cpus,
                rec.spec.memory_mib,
                rec.spec.ports.len(),
                if rec.spec.fullscreen {
                    ", full screen"
                } else {
                    ""
                },
                rec.spec
                    .resolution
                    .map(|(w, h)| format!(", {w}x{h}"))
                    .unwrap_or_default()
            );
            Ok(())
        }
        Cmd::Usb => {
            let list = usb::list();
            if list.is_empty() {
                println!("no USB devices found");
            }
            for d in list {
                println!("{:04x}:{:04x}  {}", d.vendor, d.product, d.name);
                if let Some(n) = d.note {
                    println!("           {n}");
                }
            }
            Ok(())
        }
        Cmd::Resize { name, size } => {
            let (from, to) = settings::resize(&name, ops::parse_mib(&size)?)?;
            println!(
                "grew `{name}`'s disk from {} to {} GiB; extend the partition inside the guest \
                 to use it (Windows: Disk Management > Extend Volume)",
                from / 1024,
                to / 1024
            );
            Ok(())
        }
        Cmd::Snapshot { name, cmd } => match cmd.unwrap_or(SnapshotCmd::List) {
            SnapshotCmd::List => {
                let list = settings::snapshots(&name)?;
                if list.is_empty() {
                    println!("`{name}` has no snapshots");
                }
                for s in list {
                    println!("{}  (taken {})", s.name, s.taken);
                }
                Ok(())
            }
            SnapshotCmd::Take { snapshot } => {
                settings::snapshot_create(&name, &snapshot)?;
                println!("took snapshot `{snapshot}` of `{name}`");
                Ok(())
            }
            SnapshotCmd::Restore { snapshot } => {
                settings::snapshot_restore(&name, &snapshot)?;
                println!("`{name}` is back to snapshot `{snapshot}`");
                Ok(())
            }
            SnapshotCmd::Delete { snapshot } => {
                settings::snapshot_delete(&name, &snapshot)?;
                println!("deleted snapshot `{snapshot}` of `{name}`");
                Ok(())
            }
        },
        Cmd::Eject { name } => {
            let n = ops::eject(&name)?;
            println!("ejected {n} installer disk(s) from `{name}`; it now boots its own disk");
            Ok(())
        }
        Cmd::Logs { name } => {
            store::load(&name)?;
            print!(
                "{}",
                std::fs::read_to_string(store::log_path(&name)).unwrap_or_default()
            );
            Ok(())
        }
        Cmd::Remove { name, force } => {
            ops::remove(&name, force)?;
            println!("removed `{name}`");
            Ok(())
        }
        Cmd::Run {
            image,
            tune,
            persist,
            dry_run,
            command,
        } => {
            let host = Host::detect();
            let mut opts = tune.into_options(format!("run-{}", std::process::id()), image)?;
            opts.ephemeral = !persist;
            opts.command = command;
            let rec = ops::create(&opts, &host, false, &mut say)?;
            let cleanup = || {
                let _ = std::fs::remove_dir_all(store::vm_dir(&rec.spec.name));
            };
            if rec.backend == "native" {
                if dry_run {
                    cleanup();
                    bail!("--dry-run shows QEMU command lines; the native engine has none");
                }
                say(format!(
                    "booting on bContain's own hypervisor ({}): {} vCPU, {} MiB",
                    bvmm::probe().accel,
                    rec.spec.cpus,
                    rec.spec.memory_mib
                ));
                let code = native::run_attached(&rec.spec, true);
                cleanup();
                std::process::exit(code?);
            }
            let q = qemu::Qemu::locate()?;
            if dry_run {
                println!(
                    "{} {}",
                    q.binary.display(),
                    qemu::shell_join(&q.args(&rec.spec, true)?)
                );
            } else {
                say(format!(
                    "booting with {} acceleration: {} vCPU, {} MiB",
                    q.accel.qemu_name(),
                    rec.spec.cpus,
                    rec.spec.memory_mib
                ));
                ops::gpu_acquire(&rec.spec)?;
                let code = q.run_foreground(&rec.spec);
                if let Err(e) = ops::gpu_release(&rec.spec) {
                    eprintln!("warning: {e:#}");
                }
                cleanup();
                std::process::exit(code?);
            }
            cleanup();
            Ok(())
        }
        Cmd::Pull { image } => {
            match ops::resolve_image(&image, &mut say)? {
                ops::Resolved::Oci(dir) => println!("{}", dir.display()),
                ops::Resolved::Disk(p, _) => println!("{} is a local disk image", p.display()),
            }
            Ok(())
        }
        Cmd::Images => {
            let dir = store::images_dir();
            let mut names: Vec<String> = std::fs::read_dir(&dir)
                .map(|it| {
                    it.flatten()
                        .filter_map(|e| e.file_name().into_string().ok())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            for n in names {
                println!("{n}");
            }
            Ok(())
        }
        Cmd::Kernel { cmd } => kernel(cmd),
        Cmd::Gpu { cmd } => gpu_cmd(cmd.unwrap_or(GpuCmd::Status)),
        Cmd::Diagnose { name } => diagnose::run(&name, &mut |l| println!("{l}"), &mut |t| {
            eprintln!("  (trying: {t}…)")
        }),
        Cmd::HypervisorTest => {
            println!("Testing bContain's own hypervisor on this machine:");
            match bvmm::self_test(&mut |step| println!("  - {step}")) {
                Ok(()) => {
                    println!("PASS: bContain's hypervisor works here. Try: bcontain run --engine native alpine:3.20 -- uname -a");
                    Ok(())
                }
                Err(e) => bail!("FAIL: {e}"),
            }
        }
        Cmd::NativeRun { name } => {
            let rec = store::load(&name)?;
            let code = native::run_attached(&rec.spec, false)?;
            std::process::exit(code);
        }
        Cmd::Ui { port, open } => web::serve(port, open),
    }
}

fn doctor(json: bool) -> Result<()> {
    let host = Host::detect();
    let qemu = qemu::Qemu::locate();
    if json {
        let v = serde_json::json!({
            "host": host,
            "checks": host.checks(),
            "qemu": qemu.as_ref().ok().map(|q| q.binary.display().to_string()),
            "kernel": oci::find_kernel(None).ok().map(|p| p.display().to_string()),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("bContain doctor: reference build is RTX 3080 Ti / Core i9 / 32 GB (or better)\n");
    println!("  Host OS      {}", host.os);
    for c in host.checks() {
        println!(
            "  {} {:<11} {}",
            if c.ok { "✔" } else { "✘" },
            c.item,
            c.found
        );
        if !c.ok {
            println!("                wanted: {}", c.wanted);
        }
        if let Some(fix) = c.fix {
            println!("                fix:    {fix}");
        }
    }
    match &qemu {
        Ok(q) => println!(
            "  ✔ QEMU        {} ({})",
            q.version().unwrap_or_else(|| "unknown version".into()),
            q.binary.display()
        ),
        Err(e) => println!("  ✘ QEMU        {e:#}"),
    }
    match oci::find_kernel(None) {
        Ok(k) => println!("  ✔ Containers  ready (kernel {})", k.display()),
        Err(_) => println!(
            "  • Containers  optional kernel not installed (only for container images, not ISOs or disks): `bcontain kernel fetch` or Install in the app"
        ),
    }
    let p = bvmm::probe();
    println!(
        "  {} bvmm        bContain's own hypervisor (experimental, containers): {}",
        if p.usable { "•" } else { "✘" },
        if p.usable {
            format!(
                "{} available; check it with `bcontain hypervisor-test`",
                p.accel
            )
        } else {
            format!("{} not available", p.accel)
        }
    );
    if host.hugepages_free_mib > 0 {
        println!("  ✔ Hugepages   {} MiB free", host.hugepages_free_mib);
    } else if cfg!(target_os = "linux") {
        println!("  • Hugepages   none reserved (optional: `sudo sysctl vm.nr_hugepages=8192` for 16 GiB)");
    }
    println!(
        "\n  Verdict: {}",
        if host.meets_baseline() && qemu.is_ok() {
            "meets the reference build. Ready."
        } else if qemu.is_ok() {
            "below the reference build in places; bContain still runs, sized to this host."
        } else {
            "QEMU is missing; fix that first."
        }
    );
    Ok(())
}

fn gpu_cmd(cmd: GpuCmd) -> Result<()> {
    let sys = gpu::Sysfs::default();
    match cmd {
        GpuCmd::Status => {
            let gpus = sys.gpus();
            println!("GPUs:");
            if gpus.is_empty() {
                println!("  (none found in /sys/bus/pci)");
            }
            for g in &gpus {
                println!(
                    "  {}  {}  driver {}{}{}",
                    g.address,
                    g.name,
                    g.driver.as_deref().unwrap_or("none"),
                    if g.boot_vga {
                        "  [drives the screen]"
                    } else {
                        ""
                    },
                    g.iommu_group
                        .map(|n| format!("  IOMMU group {n}"))
                        .unwrap_or_default()
                );
            }
            let candidate = sys.passthrough_candidate();
            println!("\nPassthrough readiness:");
            let checks = gpu::readiness(&sys, candidate.as_ref());
            for c in &checks {
                println!(
                    "  {} {:<14} {}",
                    if c.ok { "✔" } else { "✘" },
                    c.item,
                    c.detail
                );
                if let Some(f) = &c.fix {
                    println!("                   fix: {f}");
                }
            }
            if checks.iter().all(|c| c.ok) {
                let g = candidate.expect("ready implies a GPU");
                println!(
                    "\nReady: `sudo bcontain create bazzite bazzite-nvidia.iso --gpu` gives {} to the VM.",
                    g.name
                );
            }
            Ok(())
        }
        GpuCmd::Release { name } => {
            let rec = store::load(&name)?;
            ops::gpu_release(&rec.spec)?;
            println!("gave the GPU back to the host");
            Ok(())
        }
        GpuCmd::SetRom { name, file } => {
            let mut rec = store::load(&name)?;
            let gpu = rec
                .spec
                .gpu
                .as_mut()
                .context("that VM has no GPU passthrough")?;
            let data =
                std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            if data.len() < 512 || data[0] != 0x55 || data[1] != 0xaa {
                bail!(
                    "{} is not a video BIOS image (no 55 AA signature)",
                    file.display()
                );
            }
            let dest = store::vm_dir(&name).join("vbios.rom");
            std::fs::write(&dest, &data)?;
            gpu.rom = Some(dest);
            store::save(&rec)?;
            println!("`{name}` will use that video BIOS from its next start");
            Ok(())
        }
        GpuCmd::DumpVbios { out } => {
            let g = sys.passthrough_candidate().context("no GPU to read")?;
            let n = gpu::dump_vbios(&sys, &g.address, &out)?;
            println!(
                "saved {} KiB from {} to {}",
                n / 1024,
                g.address,
                out.display()
            );
            Ok(())
        }
    }
}

fn kernel(cmd: KernelCmd) -> Result<()> {
    let target = store::kernel_dir().join("vmlinuz");
    match cmd {
        KernelCmd::Path => {
            println!("{}", oci::find_kernel(None)?.display());
        }
        KernelCmd::Install { path } => {
            std::fs::create_dir_all(store::kernel_dir())?;
            std::fs::copy(&path, &target)?;
            println!("installed {}", target.display());
        }
        KernelCmd::Fetch { image } => {
            let path = ops::fetch_kernel(&image, &mut say)?;
            println!("installed {}", path.display());
        }
    }
    Ok(())
}
