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
pub const IMAGE_EXTS: [&str; 10] = [
    "iso", "qcow2", "img", "raw", "vhdx", "vhd", "vmdk", "vdi", "tar", "bz2",
];

/// Open `url` in a chromeless app window (Edge/Chrome/Chromium `--app`).
/// Returns the window's process when one was started, so the caller can
/// quit when the window closes; otherwise falls back to the default browser.
pub fn open_app_window(url: &str) -> Option<Child> {
    let profile = crate::store::data_dir().join("ui-profile");
    for exe in browser_candidates() {
        let child = helper(&mut Command::new(&exe))
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
    let r = helper(Command::new("cmd").args(["/C", "start", "", url]))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    #[cfg(not(windows))]
    let r = helper(Command::new("xdg-open").arg(url))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
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
            $d.Filter='VM and container images|*.iso;*.qcow2;*.img;*.raw;*.vhdx;*.vhd;*.vmdk;*.vdi;*.tar;*.bz2|All files|*.*';\
            if($d.ShowDialog($o) -eq 'OK'){$d.FileName}";
        let out =
            helper(Command::new("powershell").args(["-NoProfile", "-STA", "-Command", script]))
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
            match helper(Command::new(exe).args(&args)).output() {
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

/// The Windows share a VM's shared folder is published as.
pub fn share_name(vm: &str) -> String {
    format!("bcontain-{vm}")
}

#[cfg(any(windows, test))]
/// Standard base64, for PowerShell's -EncodedCommand.
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() {
                T[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

#[cfg(test)]
mod b64_tests {
    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
        // What `[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes('dir'))` gives.
        assert_eq!(super::encode_powershell("dir"), "ZABpAHIA");
    }
}

#[cfg(any(windows, test))]
/// A PowerShell script for `powershell -EncodedCommand`: UTF-16LE, base64.
/// Paths and names travel inside it without any quoting by the shell.
pub fn encode_powershell(script: &str) -> String {
    base64(
        &script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    )
}

/// Run a PowerShell script as administrator: Windows shows its UAC prompt.
/// Declining it is an error.
#[cfg(windows)]
fn run_elevated(script: &str) -> Result<()> {
    let outer = format!(
        "$p = Start-Process powershell -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
         -ArgumentList '-NoProfile','-EncodedCommand','{}'; exit $p.ExitCode",
        encode_powershell(script)
    );
    let out =
        helper(Command::new("powershell").args(["-NoProfile", "-Command", &outer])).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "sharing needs administrator approval, which was declined or failed ({})",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Publish `dir` as a Windows share that only this user can open, so a VM
/// reaches it at \\10.0.2.2\bcontain-NAME. `None` removes the share.
#[cfg(windows)]
pub fn windows_share(vm: &str, dir: Option<&Path>) -> Result<()> {
    let q = |s: &str| s.replace('\'', "''");
    let name = share_name(vm);
    let user = format!(
        "{}\\{}",
        std::env::var("USERDOMAIN").unwrap_or_default(),
        std::env::var("USERNAME").unwrap_or_default()
    );
    let mut script = format!(
        "$ErrorActionPreference='Stop'; \
         if (Get-SmbShare -Name '{n}' -ErrorAction SilentlyContinue) {{ Remove-SmbShare -Name '{n}' -Force }}; ",
        n = q(&name)
    );
    if let Some(dir) = dir {
        script.push_str(&format!(
            "New-SmbShare -Name '{}' -Path '{}' -FullAccess '{}' | Out-Null",
            q(&name),
            q(&dir.display().to_string()),
            q(&user)
        ));
    }
    run_elevated(&script)
}

#[cfg(not(windows))]
pub fn windows_share(_vm: &str, _dir: Option<&Path>) -> Result<()> {
    Ok(())
}

/// Samba's smbd, which QEMU runs for a Linux host's shared folders.
pub fn smbd() -> Option<PathBuf> {
    ["/usr/sbin/smbd", "/usr/bin/smbd", "/usr/local/sbin/smbd"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

/// Show the OS "choose folder" dialog. `Ok(None)` means the user cancelled.
pub fn pick_folder() -> Result<Option<String>> {
    #[cfg(windows)]
    {
        let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8;\
            Add-Type -AssemblyName System.Windows.Forms;\
            $o=New-Object System.Windows.Forms.Form -Property @{TopMost=$true};\
            $d=New-Object System.Windows.Forms.FolderBrowserDialog;\
            $d.Description='Choose where to keep this VM''s disks';\
            $d.ShowNewFolderButton=$true;\
            if($d.ShowDialog($o) -eq 'OK'){$d.SelectedPath}";
        let out =
            helper(Command::new("powershell").args(["-NoProfile", "-STA", "-Command", script]))
                .output()?;
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok((!path.is_empty()).then_some(path))
    }
    #[cfg(not(windows))]
    {
        let home = home().display().to_string();
        let attempts: [(&str, Vec<&str>); 2] = [
            (
                "zenity",
                vec![
                    "--file-selection",
                    "--directory",
                    "--title=Choose where to keep this VM's disks",
                ],
            ),
            ("kdialog", vec!["--getexistingdirectory", &home]),
        ];
        for (exe, args) in attempts {
            match helper(Command::new(exe).args(&args)).output() {
                Ok(out) => {
                    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    return Ok((out.status.success() && !path.is_empty()).then_some(path));
                }
                Err(_) => continue,
            }
        }
        anyhow::bail!(
            "no folder dialog available: install zenity (GNOME) or kdialog (KDE), or type the path"
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
    if crate::ops::is_steamos_image(p) {
        return Some("SteamOS recovery image".into());
    }
    Some(match image::sniff(p).ok()? {
        ImageKind::Disk(image::DiskFormat::Iso) => "ISO installer / live disc".into(),
        // Raw is what sniff says of any file it does not recognise; a disk
        // is at least a megabyte.
        ImageKind::Disk(image::DiskFormat::Raw) => {
            if std::fs::metadata(p).is_ok_and(|m| m.len() < 1 << 20) {
                return None;
            }
            "raw disk".into()
        }
        ImageKind::Disk(f) => format!(
            "{} disk",
            f.qemu_name().to_uppercase().replace("VPC", "VHD")
        ),
        ImageKind::OciArchive | ImageKind::OciLayout => "container image".into(),
        ImageKind::Bzip2 => "compressed disk image (bzip2)".into(),
    })
}

/// A VM name derived from a file name or image reference.
pub fn suggest_name(image: &str) -> String {
    let last = image.rsplit(['/', '\\']).next().unwrap_or(image);
    let base = match last.rsplit_once('.') {
        Some((stem, ext)) if IMAGE_EXTS.contains(&ext.to_lowercase().as_str()) => {
            stem.strip_suffix(".img").unwrap_or(stem)
        }
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

/// Prepare a helper process (qemu-img, nvidia-smi, a browser...) to start
/// safely from the double-click app as well as from a terminal.
///
/// The app detaches from its console (see [`release_console_if_owned`]),
/// which leaves its standard handles invalid. A child that inherits them
/// fails to start with "The handle is invalid. (os error 6)". So stdin is
/// always null here; callers capture or null stdout/stderr (`.output()`
/// does). On Windows the child also gets no console window, so console
/// tools don't flash one.
pub fn helper(cmd: &mut Command) -> &mut Command {
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
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
