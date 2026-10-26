// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Desktop integration for the double-click experience: the app window,
//! the native "choose a file" dialog, and finding images already on this PC.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::Result;
use serde::Serialize;

use crate::image::{self, ImageKind};

/// File extensions worth offering as images.
const IMAGE_EXTS: [&str; 9] = [
    "iso", "qcow2", "img", "raw", "vhdx", "vhd", "vmdk", "vdi", "tar",
];

/// Open `url` in a chromeless app window (Edge/Chrome/Chromium `--app`).
/// Returns the window's process when one was started, so the caller can
/// quit when the window closes; otherwise falls back to the default browser.
pub fn open_app_window(url: &str) -> Option<Child> {
    let profile = crate::store::data_dir().join("ui-profile");
    for exe in browser_candidates() {
        let child = Command::new(&exe)
            .arg(format!("--app={url}"))
            .arg(format!("--user-data-dir={}", profile.display()))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--window-size=1320,900",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(c) = child {
            return Some(c);
        }
    }
    #[cfg(windows)]
    let r = no_window(Command::new("cmd").args(["/C", "start", "", url])).spawn();
    #[cfg(not(windows))]
    let r = Command::new("xdg-open").arg(url).spawn();
    if r.is_err() {
        eprintln!("could not open a browser; visit {url}");
    }
    None
}

fn browser_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    #[cfg(windows)]
    {
        for var in ["ProgramFiles(x86)", "ProgramFiles", "LocalAppData"] {
            if let Some(base) = std::env::var_os(var) {
                let base = PathBuf::from(base);
                v.push(base.join(r"Microsoft\Edge\Application\msedge.exe"));
                v.push(base.join(r"Google\Chrome\Application\chrome.exe"));
            }
        }
        v.retain(|p| p.is_file());
    }
    #[cfg(not(windows))]
    {
        for name in [
            "chromium",
            "chromium-browser",
            "google-chrome",
            "google-chrome-stable",
            "microsoft-edge",
        ] {
            if let Some(path) = std::env::var_os("PATH") {
                if let Some(p) = std::env::split_paths(&path)
                    .map(|d| d.join(name))
                    .find(|p| p.is_file())
                {
                    v.push(p);
                }
            }
        }
    }
    v
}

/// Show the OS "open file" dialog. `Ok(None)` means the user cancelled.
pub fn pick_file() -> Result<Option<String>> {
    #[cfg(windows)]
    {
        let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8;\
            Add-Type -AssemblyName System.Windows.Forms;\
            $o=New-Object System.Windows.Forms.Form -Property @{TopMost=$true};\
            $d=New-Object System.Windows.Forms.OpenFileDialog;\
            $d.Title='Choose a virtual image';\
            $d.Filter='VM and container images|*.iso;*.qcow2;*.img;*.raw;*.vhdx;*.vhd;*.vmdk;*.vdi;*.tar|All files|*.*';\
            if($d.ShowDialog($o) -eq 'OK'){$d.FileName}";
        let out =
            no_window(Command::new("powershell").args(["-NoProfile", "-STA", "-Command", script]))
                .output()?;
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok((!path.is_empty()).then_some(path))
    }
    #[cfg(not(windows))]
    {
        let pattern = IMAGE_EXTS
            .iter()
            .map(|e| format!("*.{e}"))
            .collect::<Vec<_>>()
            .join(" ");
        let attempts: [(&str, Vec<String>); 2] = [
            (
                "zenity",
                vec![
                    "--file-selection".into(),
                    "--title=Choose a virtual image".into(),
                    format!("--file-filter=VM and container images | {pattern}"),
                    "--file-filter=All files | *".into(),
                ],
            ),
            (
                "kdialog",
                vec![
                    "--getopenfilename".into(),
                    home().display().to_string(),
                    format!("{pattern}|VM and container images"),
                ],
            ),
        ];
        for (exe, args) in attempts {
            match Command::new(exe).args(&args).output() {
                Ok(out) => {
                    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    return Ok((out.status.success() && !path.is_empty()).then_some(path));
                }
                Err(_) => continue,
            }
        }
        anyhow::bail!(
            "no file dialog available: install zenity (GNOME) or kdialog (KDE), or paste the path"
        )
    }
}

#[derive(Serialize)]
pub struct Found {
    pub path: String,
    pub name: String,
    pub size_mib: u64,
    pub kind: String,
}

/// Look for images in the places people keep them. Shallow on purpose:
/// it must answer in well under a second on a laptop.
pub fn scan() -> Vec<Found> {
    let h = home();
    let mut roots = vec![
        h.join("Downloads"),
        h.join("Desktop"),
        h.join("Documents"),
        h.join("VMs"),
        h.join("Virtual Machines"),
        h.join("ISOs"),
    ];
    if cfg!(windows) {
        roots.push(PathBuf::from(r"C:\ISOs"));
        roots.push(PathBuf::from(r"C:\VMs"));
    }
    let mut out = Vec::new();
    for r in roots {
        walk(&r, 2, &mut out);
    }
    out.sort_by_key(|f| f.name.to_lowercase());
    out.dedup_by(|a, b| a.path == b.path);
    out.truncate(200);
    out
}

fn walk(dir: &Path, depth: u32, out: &mut Vec<Found>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            if depth > 0 && !e.file_name().to_string_lossy().starts_with('.') {
                walk(&p, depth - 1, out);
            }
            continue;
        }
        let ext = p
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_lowercase();
        if !IMAGE_EXTS.contains(&ext.as_str()) || meta.len() < 1024 * 1024 {
            continue;
        }
        if let Some(kind) = describe(&p) {
            out.push(Found {
                path: p.display().to_string(),
                name: e.file_name().to_string_lossy().to_string(),
                size_mib: meta.len() / (1024 * 1024),
                kind,
            });
        }
    }
}

/// A short human label for an image file ("ISO installer", "VHDX disk",
/// "container image"), or `None` if it is not something bContain can run.
pub fn describe(p: &Path) -> Option<String> {
    Some(match image::sniff(p).ok()? {
        ImageKind::Disk(image::DiskFormat::Iso) => "ISO installer / live disc".into(),
        ImageKind::Disk(image::DiskFormat::Raw) => "raw disk".into(),
        ImageKind::Disk(f) => format!(
            "{} disk",
            f.qemu_name().to_uppercase().replace("VPC", "VHD")
        ),
        ImageKind::OciArchive | ImageKind::OciLayout => "container image".into(),
    })
}

/// A VM name derived from a file name or image reference.
pub fn suggest_name(image: &str) -> String {
    let last = image.rsplit(['/', '\\']).next().unwrap_or(image);
    let base = match last.rsplit_once('.') {
        Some((stem, ext)) if IMAGE_EXTS.contains(&ext.to_lowercase().as_str()) => stem,
        _ => last,
    };
    let base = base.replace(':', "-");
    let mut s: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    s = s.trim_matches(|c| c == '-' || c == '.').to_string();
    s.truncate(40);
    if s.is_empty() {
        s = "vm".into();
    }
    let mut name = s.clone();
    let mut n = 2;
    while crate::store::exists(&name) {
        name = format!("{s}-{n}");
        n += 1;
    }
    name
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// On Windows, keep helper processes from flashing a console window.
#[cfg(windows)]
pub fn no_window(cmd: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000) // CREATE_NO_WINDOW
}

/// When bContain was started by double-clicking it, Windows gave it a
/// console window of its own. Hide it once the app window is up; it closes
/// only if no other process (e.g. a terminal you launched us from) shares it.
pub fn release_console_if_owned() {
    #[cfg(windows)]
    unsafe {
        extern "system" {
            fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
            fn FreeConsole() -> i32;
        }
        let mut pids = [0u32; 4];
        if GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) == 1 {
            FreeConsole();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        std::env::set_var(
            "BCONTAIN_HOME",
            std::env::temp_dir().join("bcontain-names-test"),
        );
        assert_eq!(
            suggest_name("/x/Ubuntu 24.04 Desktop.iso"),
            "ubuntu-24.04-desktop"
        );
        assert_eq!(suggest_name("C:\\ISOs\\Win11_23H2.iso"), "win11-23h2");
        assert_eq!(suggest_name("alpine:3.20"), "alpine-3.20");
        assert_eq!(suggest_name("ghcr.io/foo/bar:1"), "bar-1");
    }
}
