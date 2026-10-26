//! Host inspection and the reference-hardware check.
//!
//! bContain is tuned for an RTX 3080 Ti / Core i9 / 32 GB laptop "or better".
//! `bcontain doctor` reports how this host compares and what to fix.

use std::process::Command;

use serde::Serialize;

/// The reference machine bContain is tuned for.
pub mod baseline {
    pub const PHYSICAL_CORES: usize = 8;
    pub const THREADS: usize = 16;
    /// 32 GB sticks report ~31.3–31.8 GiB usable, so allow some slack.
    pub const MEMORY_GIB: f64 = 30.0;
    /// RTX 3080 Ti laptop ships 16 GB; the desktop card ships 12 GB.
    pub const VRAM_MIB: u64 = 12 * 1024;
}

#[derive(Debug, Clone, Serialize)]
pub struct Gpu {
    pub name: String,
    pub vram_mib: u64,
    pub driver: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Host {
    pub os: String,
    pub cpu: String,
    pub physical_cores: usize,
    pub threads: usize,
    pub memory_mib: u64,
    pub gpus: Vec<Gpu>,
    pub accel: String,
    pub accel_usable: bool,
    pub accel_detail: String,
    /// Free 2 MiB hugepages (Linux only; 0 elsewhere).
    pub hugepages_free_mib: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub item: &'static str,
    pub ok: bool,
    pub found: String,
    pub wanted: String,
    pub fix: Option<String>,
}

impl Host {
    pub fn detect() -> Host {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_all();
        sys.refresh_memory();
        let cpu = sys
            .cpus()
            .first()
            .map(|c| c.brand().trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown CPU".into());
        let threads = sys.cpus().len().max(1);
        let physical_cores = sys.physical_core_count().unwrap_or(threads);
        let probe = bvmm::probe();
        Host {
            os: format!(
                "{} {}",
                sysinfo::System::name().unwrap_or_else(|| std::env::consts::OS.into()),
                sysinfo::System::os_version().unwrap_or_default()
            )
            .trim()
            .to_string(),
            cpu,
            physical_cores,
            threads,
            memory_mib: sys.total_memory() / (1024 * 1024),
            gpus: nvidia_gpus(),
            accel: probe.accel.to_string(),
            accel_usable: probe.usable,
            accel_detail: probe.detail,
            hugepages_free_mib: hugepages_free_mib(),
        }
    }

    pub fn memory_gib(&self) -> f64 {
        self.memory_mib as f64 / 1024.0
    }

    pub fn checks(&self) -> Vec<Check> {
        let best_gpu = self.gpus.iter().max_by_key(|g| g.vram_mib);
        vec![
            Check {
                item: "CPU",
                ok: self.physical_cores >= baseline::PHYSICAL_CORES
                    && self.threads >= baseline::THREADS,
                found: format!(
                    "{} ({} cores / {} threads)",
                    self.cpu, self.physical_cores, self.threads
                ),
                wanted: format!(
                    "Core i9 class: >= {} cores / {} threads",
                    baseline::PHYSICAL_CORES,
                    baseline::THREADS
                ),
                fix: None,
            },
            Check {
                item: "Memory",
                ok: self.memory_gib() >= baseline::MEMORY_GIB,
                found: format!("{:.1} GiB", self.memory_gib()),
                wanted: "32 GB or more".into(),
                fix: None,
            },
            Check {
                item: "GPU",
                ok: best_gpu.is_some_and(|g| g.vram_mib >= baseline::VRAM_MIB),
                found: best_gpu
                    .map(|g| format!("{} ({} MiB, driver {})", g.name, g.vram_mib, g.driver))
                    .unwrap_or_else(|| "no NVIDIA GPU detected".into()),
                wanted: "RTX 3080 Ti class (>= 12 GB VRAM)".into(),
                fix: best_gpu.is_none().then(|| {
                    "install the NVIDIA driver so `nvidia-smi` is on PATH; on hybrid laptops \
                     select the dGPU (MUX/Advanced Optimus) for best virgl performance"
                        .into()
                }),
            },
            Check {
                item: "Accelerator",
                ok: self.accel_usable,
                found: self.accel.clone(),
                wanted: "KVM (Linux) or Windows Hypervisor Platform".into(),
                fix: (!self.accel_usable).then(|| self.accel_detail.clone()),
            },
        ]
    }

    pub fn meets_baseline(&self) -> bool {
        self.checks().iter().all(|c| c.ok)
    }
}

fn nvidia_gpus() -> Vec<Gpu> {
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total,driver_version",
            "--format=csv,noheader,nounits",
        ])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split(',').map(str::trim);
            Some(Gpu {
                name: parts.next()?.to_string(),
                vram_mib: parts.next()?.parse().ok()?,
                driver: parts.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

fn hugepages_free_mib() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let read = |f: &str| {
            std::fs::read_to_string(format!("/sys/kernel/mm/hugepages/hugepages-2048kB/{f}"))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0)
        };
        read("free_hugepages") * 2
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}
