//! Sizing presets tuned for the RTX 3080 Ti / Core i9 / 32 GB reference laptop.
//!
//! Presets are computed from the *detected* host, so a bigger machine
//! ("or better") gets proportionally bigger guests while the host always keeps
//! enough cores and RAM to stay responsive.

use serde::Serialize;

use crate::engine::Display;
use crate::host::Host;

/// RAM the host keeps for itself no matter what.
const HOST_RESERVE_MIB: u64 = 6 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Preset {
    /// Default desktop VM: half the threads (max 16), 16 GiB, virgl 3D.
    Rtx3080ti,
    /// Small background VM: 4 vCPU, 4 GiB, no 3D.
    Light,
    /// Everything the host can spare.
    Max,
    /// Container micro-VM: 4 vCPU, 2 GiB, headless serial console.
    Container,
}

#[derive(Debug, Clone, Serialize)]
pub struct Sizing {
    pub cpus: u32,
    pub memory_mib: u64,
    pub display: Display,
    pub hugepages: bool,
}

impl Preset {
    pub const ALL: [Preset; 4] = [
        Preset::Rtx3080ti,
        Preset::Light,
        Preset::Max,
        Preset::Container,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Preset::Rtx3080ti => "rtx3080ti",
            Preset::Light => "light",
            Preset::Max => "max",
            Preset::Container => "container",
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<Preset> {
        Preset::ALL
            .into_iter()
            .find(|p| p.name() == s)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown preset `{s}` (expected one of: {})",
                    Preset::ALL.map(|p| p.name()).join(", ")
                )
            })
    }

    pub fn sizing(self, host: &Host) -> Sizing {
        let threads = host.threads as u32;
        let spare_mib = host.memory_mib.saturating_sub(HOST_RESERVE_MIB).max(1024);
        let (cpus, memory_mib, display) = match self {
            Preset::Rtx3080ti => (
                (threads / 2).clamp(2, 16),
                spare_mib.min(16 * 1024),
                default_3d(),
            ),
            Preset::Light => (4, 4 * 1024, Display::Basic),
            Preset::Max => (threads.saturating_sub(2), spare_mib, default_3d()),
            Preset::Container => (4, 2 * 1024, Display::Headless),
        };
        let memory_mib = memory_mib.min(spare_mib);
        Sizing {
            cpus: cpus.clamp(1, threads.max(1)),
            memory_mib,
            display,
            // Only ask for hugepages when they are already reserved; QEMU fails
            // to start rather than falling back if the pool is short.
            hugepages: host.hugepages_free_mib >= memory_mib,
        }
    }
}

/// virgl is mature on Linux hosts. QEMU's Windows builds can do it too, but
/// support depends on the build, so Windows starts with the safe option and
/// `--display virgl` opts in.
fn default_3d() -> Display {
    if cfg!(windows) {
        Display::Basic
    } else {
        Display::Virgl
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn laptop() -> Host {
        // i9-12900HK + 32 GB + RTX 3080 Ti Laptop
        Host {
            os: "test".into(),
            cpu: "12th Gen Intel(R) Core(TM) i9-12900HK".into(),
            physical_cores: 14,
            threads: 20,
            memory_mib: 32 * 1024 - 400,
            gpus: vec![],
            accel: "KVM".into(),
            accel_usable: true,
            accel_detail: String::new(),
            hugepages_free_mib: 0,
        }
    }

    #[test]
    fn reference_laptop_sizing() {
        let s = Preset::Rtx3080ti.sizing(&laptop());
        assert_eq!(s.cpus, 10);
        assert_eq!(s.memory_mib, 16 * 1024);
        assert!(!s.hugepages);

        let m = Preset::Max.sizing(&laptop());
        assert_eq!(m.cpus, 18);
        assert_eq!(m.memory_mib, 32 * 1024 - 400 - HOST_RESERVE_MIB);
    }

    #[test]
    fn small_host_is_clamped() {
        let mut h = laptop();
        h.threads = 2;
        h.memory_mib = 4096;
        let s = Preset::Light.sizing(&h);
        assert_eq!(s.cpus, 2);
        assert_eq!(s.memory_mib, 1024);
    }
}
