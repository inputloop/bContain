// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! bcontain: a standalone VM + container runner for Windows and Linux,
//! tuned for RTX 3080 Ti / Core i9 / 32 GB laptops (or better).

mod desktop;
mod engine;
mod host;
mod image;
mod oci;
mod ops;
mod preset;
mod qemu;
mod registry;
mod store;
mod web;

use std::path::PathBuf;

use anyhow::Result;
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
        /// Size of the blank disk created next to an ISO (e.g. 64G; 0 for none).
        #[arg(long)]
        disk_size: Option<String>,
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
    /// Forward a host port to the guest, HOST:GUEST (repeatable).
    #[arg(short = 'p', long = "port")]
    ports: Vec<String>,
    /// Extra environment for containers, KEY=VALUE (repeatable).
    #[arg(short = 'e', long = "env")]
    env: Vec<String>,
    /// Guest kernel for containers (bzImage).
    #[arg(long)]
    kernel: Option<PathBuf>,
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
            ephemeral: false,
            ports: self
                .ports
                .iter()
                .map(|p| ops::parse_port(p))
                .collect::<Result<_>>()?,
            command: Vec::new(),
            env: self.env,
            kernel: self.kernel,
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
            command,
        } => {
            let host = Host::detect();
            let mut opts = tune.into_options(name, image)?;
            opts.command = command;
            opts.disk_mib = disk_size.as_deref().map(ops::parse_mib).transpose()?;
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
                let code = hv.run_foreground(&rec.spec)?;
                std::process::exit(code);
            }
            let r = ops::start(&name)?;
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
            let q = qemu::Qemu::locate()?;
            println!(
                "\n{} {}",
                q.binary.display(),
                qemu::shell_join(&q.args(&rec.spec, false)?)
            );
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
                let code = q.run_foreground(&rec.spec);
                let _ = std::fs::remove_dir_all(store::vm_dir(&rec.spec.name));
                std::process::exit(code?);
            }
            let _ = std::fs::remove_dir_all(store::vm_dir(&rec.spec.name));
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
        Ok(k) => println!("  ✔ Kernel      {} (for containers)", k.display()),
        Err(_) => println!(
            "  • Kernel      not installed; `bcontain kernel fetch` enables container images"
        ),
    }
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
