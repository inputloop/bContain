// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! On-disk state: one folder per VM under the bContain data directory.
//!
//! ```text
//! <data>/vms/<name>/vm.json    definition (VmSpec + metadata)
//! <data>/vms/<name>/run.json   present while the VM runs (pid, QMP port)
//! <data>/vms/<name>/qemu.log   backend output
//! <data>/images/<name>/        OCI image layout for pulled/imported images
//! <data>/kernel/               guest kernel for container micro-VMs
//! ```
//!
//! `<data>` is `$BCONTAIN_HOME`, else `%LOCALAPPDATA%\bcontain` on Windows or
//! `~/.local/share/bcontain` on Linux.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::engine::{Hypervisor, Running, VmSpec};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmRecord {
    pub spec: VmSpec,
    pub preset: String,
    pub backend: String,
    /// Unix seconds.
    pub created: u64,
    /// Image the VM was created from, for display.
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Stopped,
}

pub fn data_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("BCONTAIN_HOME") {
        return PathBuf::from(p);
    }
    // GPU passthrough needs root. Under `sudo`, keep using the invoking
    // user's VMs instead of a separate set under /root.
    if let Some(home) = sudo_user_home() {
        return home.join(".local/share/bcontain");
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("bcontain")
}

fn sudo_user_home() -> Option<PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let user = std::env::var("SUDO_USER")
        .ok()
        .filter(|u| !u.is_empty() && u != "root")?;
    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    passwd
        .lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.len() > 5 && f[0] == user)
        .map(|f| PathBuf::from(f[5]))
}

pub fn vm_dir(name: &str) -> PathBuf {
    data_dir().join("vms").join(name)
}

pub fn images_dir() -> PathBuf {
    data_dir().join("images")
}

pub fn kernel_dir() -> PathBuf {
    data_dir().join("kernel")
}

pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.');
    if !ok {
        bail!("invalid name `{name}`: use 1-64 letters, digits, `-`, `_` or `.`");
    }
    Ok(())
}

pub fn save(rec: &VmRecord) -> Result<()> {
    let dir = vm_dir(&rec.spec.name);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    write_json(&dir.join("vm.json"), rec)
}

pub fn load(name: &str) -> Result<VmRecord> {
    validate_name(name)?;
    let path = vm_dir(name).join("vm.json");
    let text = fs::read_to_string(&path).with_context(|| format!("no VM named `{name}`"))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn exists(name: &str) -> bool {
    vm_dir(name).join("vm.json").exists()
}

pub fn list() -> Result<Vec<VmRecord>> {
    let dir = data_dir().join("vms");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        if let Some(name) = e.file_name().to_str() {
            if let Ok(rec) = load(name) {
                out.push(rec);
            }
        }
    }
    out.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
    Ok(out)
}

pub fn remove(name: &str) -> Result<()> {
    validate_name(name)?;
    fs::remove_dir_all(vm_dir(name)).with_context(|| format!("removing VM `{name}`"))
}

pub fn set_running(name: &str, r: &Running) -> Result<()> {
    write_json(&vm_dir(name).join("run.json"), r)
}

pub fn clear_running(name: &str) {
    let _ = fs::remove_file(vm_dir(name).join("run.json"));
}

/// The running handle, if the VM is actually still alive. Stale handles left
/// behind by a crash or host reboot are cleaned up here.
pub fn running(name: &str, hv: &dyn Hypervisor) -> Option<Running> {
    let text = fs::read_to_string(vm_dir(name).join("run.json")).ok()?;
    let r: Running = serde_json::from_str(&text).ok()?;
    if hv.is_alive(&r) {
        Some(r)
    } else {
        clear_running(name);
        None
    }
}

pub fn state(name: &str, hv: &dyn Hypervisor) -> State {
    if running(name, hv).is_some() {
        State::Running
    } else {
        State::Stopped
    }
}

pub fn log_path(name: &str) -> PathBuf {
    vm_dir(name).join("qemu.log")
}

fn write_json<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::validate_name;

    #[test]
    fn names() {
        assert!(validate_name("ubuntu-24.04").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("../etc").is_err());
        assert!(validate_name(".hidden").is_err());
        assert!(validate_name("a b").is_err());
    }
}
