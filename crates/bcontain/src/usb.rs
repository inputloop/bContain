// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Host USB devices a VM can take over (QEMU `usb-host`), matched by vendor
//! and product ID so the device can be plugged in before or after the VM
//! starts.

#[cfg(any(not(windows), test))]
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A device a VM is given, as stored in its spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbDevice {
    pub vendor: u16,
    pub product: u16,
    /// For people: "Xbox Wireless Controller".
    #[serde(default)]
    pub name: String,
}

/// A device plugged into the host now.
#[derive(Debug, Clone, Serialize)]
pub struct HostUsb {
    pub vendor: u16,
    pub product: u16,
    pub name: String,
    /// What has to happen before QEMU can open it, if anything.
    pub note: Option<String>,
}

fn hex(s: &str) -> Option<u16> {
    u16::from_str_radix(s.trim(), 16).ok()
}

/// Devices under a sysfs tree (`/sys` on a real host), hubs left out.
#[cfg(any(not(windows), test))]
pub fn list_linux(sys: &Path, dev: &Path) -> Vec<HostUsb> {
    let Ok(entries) = std::fs::read_dir(sys.join("bus/usb/devices")) else {
        return Vec::new();
    };
    let read = |d: &Path, f: &str| {
        std::fs::read_to_string(d.join(f))
            .ok()
            .map(|s| s.trim().to_string())
    };
    let mut out: Vec<HostUsb> = entries
        .flatten()
        .map(|e| e.path())
        .filter_map(|d| {
            let (vendor, product) = (hex(&read(&d, "idVendor")?)?, hex(&read(&d, "idProduct")?)?);
            if read(&d, "bDeviceClass").as_deref() == Some("09") {
                return None; // a hub
            }
            let name = [read(&d, "manufacturer"), read(&d, "product")]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            // QEMU opens /dev/bus/usb/BBB/DDD read-write.
            let node = match (read(&d, "busnum"), read(&d, "devnum")) {
                (Some(b), Some(n)) => {
                    let (b, n): (u32, u32) = (b.parse().ok()?, n.parse().ok()?);
                    Some(dev.join(format!("bus/usb/{b:03}/{n:03}")))
                }
                _ => None,
            };
            let note = match node {
                Some(n) if std::fs::OpenOptions::new().read(true).write(true).open(&n).is_err() => {
                    Some(format!(
                        "QEMU can't open {} as this user: run bContain with sudo, or add a udev rule giving you access to {vendor:04x}:{product:04x}",
                        n.display()
                    ))
                }
                _ => None,
            };
            Some(HostUsb {
                vendor,
                product,
                name: if name.is_empty() { format!("USB device {vendor:04x}:{product:04x}") } else { name },
                note,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.vendor == b.vendor && a.product == b.product);
    out
}

/// Devices on a Windows host, from Windows' device list. QEMU reaches USB
/// through libusb, which needs the device on the WinUSB driver.
#[cfg(windows)]
fn list_windows() -> Vec<HostUsb> {
    let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
        Get-CimInstance Win32_PnPEntity | Where-Object { $_.DeviceID -like 'USB\\VID_*' -and $_.DeviceID -notlike '*&MI_*' } | \
        ForEach-Object { '{0}|{1}|{2}' -f $_.DeviceID, $_.Service, $_.Name }";
    let Ok(out) = crate::desktop::helper(std::process::Command::new("powershell").args([
        "-NoProfile",
        "-Command",
        script,
    ]))
    .output() else {
        return Vec::new();
    };
    let mut list: Vec<HostUsb> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '|');
            let (id, service, name) = (f.next()?, f.next()?, f.next()?.trim());
            let id = id.to_ascii_uppercase();
            let vendor = hex(id.split("VID_").nth(1)?.get(..4)?)?;
            let product = hex(id.split("PID_").nth(1)?.get(..4)?)?;
            if name.to_lowercase().contains("hub") {
                return None;
            }
            let note = (!service.eq_ignore_ascii_case("WinUSB")).then(|| {
                "Needs the WinUSB driver first (install it with Zadig, zadig.akeo.ie). While it has WinUSB, Windows itself can't use the device.".to_string()
            });
            Some(HostUsb {
                vendor,
                product,
                name: name.to_string(),
                note,
            })
        })
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list.dedup_by(|a, b| a.vendor == b.vendor && a.product == b.product);
    list
}

/// USB devices plugged into this host.
pub fn list() -> Vec<HostUsb> {
    #[cfg(windows)]
    {
        list_windows()
    }
    #[cfg(not(windows))]
    {
        list_linux(std::path::Path::new("/sys"), std::path::Path::new("/dev"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(root: &Path, id: &str, fields: &[(&str, &str)]) {
        let d = root.join("bus/usb/devices").join(id);
        std::fs::create_dir_all(&d).unwrap();
        for (k, v) in fields {
            std::fs::write(d.join(k), format!("{v}\n")).unwrap();
        }
    }

    #[test]
    fn lists_devices_not_hubs() {
        let root = std::env::temp_dir().join(format!("bcontain-usb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        device(
            &root,
            "usb1",
            &[
                ("idVendor", "1d6b"),
                ("idProduct", "0002"),
                ("bDeviceClass", "09"),
                ("product", "xHCI Host Controller"),
            ],
        );
        device(
            &root,
            "1-2",
            &[
                ("idVendor", "045e"),
                ("idProduct", "0b12"),
                ("bDeviceClass", "ef"),
                ("manufacturer", "Microsoft"),
                ("product", "Controller"),
                ("busnum", "1"),
                ("devnum", "3"),
            ],
        );
        device(
            &root,
            "1-3",
            &[
                ("idVendor", "0781"),
                ("idProduct", "5581"),
                ("bDeviceClass", "00"),
            ],
        );
        device(&root, "1-2:1.0", &[("bInterfaceClass", "ff")]); // an interface, not a device
        let l = list_linux(&root, &root.join("dev"));
        assert_eq!(l.len(), 2);
        assert_eq!(
            (l[0].vendor, l[0].product, l[0].name.as_str()),
            (0x045e, 0x0b12, "Microsoft Controller")
        );
        assert!(l[0]
            .note
            .as_deref()
            .is_some_and(|n| n.contains("bus/usb/001/003")));
        assert_eq!(l[1].name, "USB device 0781:5581");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
