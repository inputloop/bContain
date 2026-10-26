// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Identify image files by content, not by extension.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskFormat {
    Iso,
    Qcow2,
    Vhdx,
    Vmdk,
    /// Legacy VHD (Hyper-V / Virtual PC); QEMU calls it `vpc`.
    Vhd,
    Vdi,
    Raw,
}

impl DiskFormat {
    /// The `format=` value QEMU expects.
    pub fn qemu_name(self) -> &'static str {
        match self {
            DiskFormat::Iso | DiskFormat::Raw => "raw",
            DiskFormat::Qcow2 => "qcow2",
            DiskFormat::Vhdx => "vhdx",
            DiskFormat::Vmdk => "vmdk",
            DiskFormat::Vhd => "vpc",
            DiskFormat::Vdi => "vdi",
        }
    }
}

/// What an image file on disk turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Disk(DiskFormat),
    /// `docker save` archive or OCI image-layout tarball.
    OciArchive,
    /// OCI image-layout directory (contains `index.json` + `blobs/`).
    OciLayout,
    /// A bzip2-compressed disk image (SteamOS's recovery image ships so).
    Bzip2,
}

pub fn sniff(path: &Path) -> Result<ImageKind> {
    if path.is_dir() {
        if path.join("index.json").exists() && path.join("blobs").is_dir() {
            return Ok(ImageKind::OciLayout);
        }
        anyhow::bail!(
            "{} is a directory but not an OCI image layout",
            path.display()
        );
    }
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = f.metadata()?.len();
    let mut head = [0u8; 512];
    let n = read_up_to(&mut f, &mut head)?;
    let head = &head[..n];

    if head.starts_with(b"QFI\xfb") {
        return Ok(ImageKind::Disk(DiskFormat::Qcow2));
    }
    if head.starts_with(b"vhdxfile") {
        return Ok(ImageKind::Disk(DiskFormat::Vhdx));
    }
    if head.starts_with(b"KDMV") || head.starts_with(b"# Disk DescriptorFile") {
        return Ok(ImageKind::Disk(DiskFormat::Vmdk));
    }
    if head.len() >= 0x44 && head[0x40..0x44] == [0x7f, 0x10, 0xda, 0xbe] {
        return Ok(ImageKind::Disk(DiskFormat::Vdi));
    }
    if head.starts_with(b"conectix") {
        return Ok(ImageKind::Disk(DiskFormat::Vhd));
    }
    // ustar magic at offset 257 means a tar archive: assume an OCI/docker image.
    if head.len() >= 262 && &head[257..262] == b"ustar" {
        return Ok(ImageKind::OciArchive);
    }
    if head.len() >= 4 && &head[..3] == b"BZh" && (b'1'..=b'9').contains(&head[3]) {
        return Ok(ImageKind::Bzip2);
    }
    if head.starts_with(&[0x1f, 0x8b]) {
        anyhow::bail!(
            "{} is gzip-compressed; decompress it first (bContain reads `docker save` \
             tarballs uncompressed)",
            path.display()
        );
    }
    // ISO 9660 primary volume descriptor at sector 16.
    if len >= 0x8006 {
        let mut pvd = [0u8; 6];
        f.seek(SeekFrom::Start(0x8000))?;
        if read_up_to(&mut f, &mut pvd)? == 6 && &pvd[1..6] == b"CD001" {
            return Ok(ImageKind::Disk(DiskFormat::Iso));
        }
    }
    // Fixed VHD carries its footer in the last 512 bytes.
    if len >= 512 {
        let mut foot = [0u8; 8];
        f.seek(SeekFrom::Start(len - 512))?;
        if read_up_to(&mut f, &mut foot)? == 8 && &foot == b"conectix" {
            return Ok(ImageKind::Disk(DiskFormat::Vhd));
        }
    }
    Ok(ImageKind::Disk(DiskFormat::Raw))
}

/// The ISO 9660 volume identifier (the disc's label), if `path` is an ISO.
pub fn iso_volume_id(path: &Path) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let mut pvd = [0u8; 72];
    f.seek(SeekFrom::Start(0x8000)).ok()?;
    if read_up_to(&mut f, &mut pvd).ok()? < 72 || &pvd[1..6] != b"CD001" {
        return None;
    }
    Some(String::from_utf8_lossy(&pvd[40..72]).trim().to_string())
}

/// A Windows installation ISO, by its disc label (Microsoft's labels start
/// with the edition family, e.g. `CCCOMA_X64FRE_EN-US_DV9`) or file name.
pub fn is_windows_installer(path: &Path) -> bool {
    const LABELS: [&str; 8] = [
        "CCCOMA_", "CCSA_", "CPBA_", "CENA_", "CPRA_", "J_CCSA", "SSS_X64", "WIN",
    ];
    if iso_volume_id(path).is_some_and(|v| LABELS.iter().any(|l| v.to_uppercase().starts_with(l))) {
        return true;
    }
    let n = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    n.ends_with(".iso")
        && (["win10", "win11", "windows", "server20"]
            .iter()
            .any(|k| n.contains(k))
            || n.starts_with("win"))
}

/// The alphaOS live ISO (inputloop/alphaOS): its builder labels the disc
/// `ALPHAOS` (`grub-mkrescue -volid ALPHAOS`); releases are named
/// `alphaos-live-*.iso`.
pub fn is_alphaos_iso(path: &Path) -> bool {
    if iso_volume_id(path).is_some_and(|v| v.eq_ignore_ascii_case("ALPHAOS")) {
        return true;
    }
    let n = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    n.starts_with("alphaos") && n.ends_with(".iso") && iso_volume_id(path).is_some()
}

/// What an ISO's El Torito boot catalog offers.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IsoBoot {
    /// A bootable x86 entry: legacy BIOS can start it.
    pub bios: bool,
    /// An EFI entry: UEFI can start it.
    pub efi: bool,
}

impl IsoBoot {
    /// Only UEFI can start this disc (SeaBIOS: "Could not read from CDROM (code 0009)").
    pub fn uefi_only(&self) -> bool {
        self.efi && !self.bios
    }
}

/// Read an ISO's El Torito boot catalog; `None` if the disc is not bootable.
pub fn iso_boot(path: &Path) -> Option<IsoBoot> {
    const PLATFORM_X86: u8 = 0;
    const PLATFORM_EFI: u8 = 0xef;
    let mut f = File::open(path).ok()?;
    // Volume descriptors start at sector 16; the boot record is type 0.
    let mut catalog_lba = None;
    for sector in 16..32u64 {
        let mut vd = [0u8; 0x4b];
        f.seek(SeekFrom::Start(sector * 2048)).ok()?;
        if read_up_to(&mut f, &mut vd).ok()? < vd.len() || &vd[1..6] != b"CD001" {
            return None;
        }
        match vd[0] {
            0 if vd[7..30].starts_with(b"EL TORITO SPECIFICATION") => {
                catalog_lba = Some(u32::from_le_bytes(vd[0x47..0x4b].try_into().unwrap()));
                break;
            }
            255 => break,
            _ => {}
        }
    }
    let mut cat = [0u8; 2048];
    f.seek(SeekFrom::Start(catalog_lba? as u64 * 2048)).ok()?;
    read_up_to(&mut f, &mut cat).ok()?;
    if cat[0] != 1 || cat[0x1e..0x20] != [0x55, 0xaa] {
        return None;
    }
    let mut boot = IsoBoot::default();
    let mut platform = cat[1];
    // The initial/default entry belongs to the validation entry's platform.
    let mut mark = |platform: u8, bootable: bool| {
        if bootable {
            boot.bios |= platform == PLATFORM_X86;
            boot.efi |= platform == PLATFORM_EFI;
        }
    };
    mark(platform, cat[0x20] == 0x88);
    // Then section headers (0x90, last one 0x91), each followed by its entries.
    let mut at = 0x40;
    while at + 32 <= cat.len() {
        match cat[at] {
            0x90 | 0x91 => {
                platform = cat[at + 1];
                let count = u16::from_le_bytes([cat[at + 2], cat[at + 3]]) as usize;
                for i in 1..=count {
                    let e = at + i * 32;
                    if e + 32 > cat.len() {
                        break;
                    }
                    mark(platform, cat[e] == 0x88);
                }
                let last = cat[at] == 0x91;
                at += (count + 1) * 32;
                if last {
                    break;
                }
            }
            _ => break,
        }
    }
    Some(boot)
}

/// What a raw disk's GPT partition table says about booting it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GptInfo {
    /// Has an EFI System Partition: UEFI can boot it.
    pub esp: bool,
    /// Has a BIOS boot partition: legacy BIOS (via GRUB) can boot it too.
    pub bios_boot: bool,
    /// Partition names, as written in the table.
    pub names: Vec<String>,
}

impl GptInfo {
    /// Only UEFI can start this disk.
    pub fn uefi_only(&self) -> bool {
        self.esp && !self.bios_boot
    }

    /// SteamOS's A/B partition layout.
    pub fn looks_like_steamos(&self) -> bool {
        let has = |n: &str| self.names.iter().any(|x| x.eq_ignore_ascii_case(n));
        has("rootfs-A") && (has("efi-A") || has("var-A"))
    }
}

/// GUIDs as stored on disk (first three fields little-endian).
const ESP_GUID: [u8; 16] = [
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
];
const BIOS_BOOT_GUID: [u8; 16] = [
    0x48, 0x61, 0x68, 0x21, 0x49, 0x64, 0x6f, 0x6e, 0x74, 0x4e, 0x65, 0x65, 0x64, 0x45, 0x46, 0x49,
];

/// Read the GPT of a raw disk image (512- or 4096-byte sectors).
pub fn gpt_info(path: &Path) -> Option<GptInfo> {
    let mut f = File::open(path).ok()?;
    for sector in [512u64, 4096] {
        let mut hdr = [0u8; 92];
        f.seek(SeekFrom::Start(sector)).ok()?;
        if read_up_to(&mut f, &mut hdr).ok()? < 92 || &hdr[..8] != b"EFI PART" {
            continue;
        }
        let le32 = |o: usize| u32::from_le_bytes(hdr[o..o + 4].try_into().unwrap());
        let entries_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let count = le32(80).min(256) as usize;
        let size = le32(84) as usize;
        if !(128..=512).contains(&size) {
            return None;
        }
        let mut table = vec![0u8; count * size];
        f.seek(SeekFrom::Start(entries_lba * sector)).ok()?;
        let n = read_up_to(&mut f, &mut table).ok()?;
        let mut info = GptInfo::default();
        for e in table[..n].chunks_exact(size) {
            let kind: [u8; 16] = e[..16].try_into().unwrap();
            if kind == [0; 16] {
                continue;
            }
            info.esp |= kind == ESP_GUID;
            info.bios_boot |= kind == BIOS_BOOT_GUID;
            let name: Vec<u16> = e[56..128]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&c| c != 0)
                .collect();
            info.names.push(String::from_utf16_lossy(&name));
        }
        return Some(info);
    }
    None
}

fn read_up_to(f: &mut File, buf: &mut [u8]) -> Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        let n = f.read(&mut buf[total..])?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("bcontain-sniff-{}-{name}", std::process::id()));
        File::create(&p).unwrap().write_all(bytes).unwrap();
        p
    }

    /// A 64 KiB disk with a GPT: header at LBA 1, entries at LBA 2.
    fn gpt_disk(parts: &[([u8; 16], &str)]) -> Vec<u8> {
        let mut d = vec![0u8; 64 * 1024];
        d[512..520].copy_from_slice(b"EFI PART");
        d[512 + 72..512 + 80].copy_from_slice(&2u64.to_le_bytes());
        d[512 + 80..512 + 84].copy_from_slice(&(parts.len() as u32).to_le_bytes());
        d[512 + 84..512 + 88].copy_from_slice(&128u32.to_le_bytes());
        for (i, (kind, name)) in parts.iter().enumerate() {
            let e = 1024 + i * 128;
            d[e..e + 16].copy_from_slice(kind);
            for (j, c) in name.encode_utf16().enumerate() {
                d[e + 56 + j * 2..e + 58 + j * 2].copy_from_slice(&c.to_le_bytes());
            }
        }
        d
    }

    fn iso_with_label(label: &str) -> Vec<u8> {
        let mut d = vec![0u8; 0x8800];
        d[0x8001..0x8006].copy_from_slice(b"CD001");
        d[0x8028..0x8048].fill(b' ');
        d[0x8028..0x8028 + label.len()].copy_from_slice(label.as_bytes());
        d
    }

    /// A minimal El Torito disc: boot record at 17, catalog at 20.
    fn eltorito(default_platform: u8, sections: &[u8]) -> Vec<u8> {
        let mut img = vec![0u8; 24 * 2048];
        let pvd = 16 * 2048;
        img[pvd] = 1;
        img[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
        let br = 17 * 2048;
        img[br + 1..br + 6].copy_from_slice(b"CD001");
        img[br + 7..br + 30].copy_from_slice(b"EL TORITO SPECIFICATION");
        img[br + 0x47..br + 0x4b].copy_from_slice(&20u32.to_le_bytes());
        let t = 18 * 2048;
        img[t] = 255;
        img[t + 1..t + 6].copy_from_slice(b"CD001");
        let c = 20 * 2048;
        img[c] = 1;
        img[c + 1] = default_platform;
        img[c + 0x1e] = 0x55;
        img[c + 0x1f] = 0xaa;
        img[c + 0x20] = 0x88;
        for (i, p) in sections.iter().enumerate() {
            let h = c + 0x40 + i * 64;
            img[h] = if i + 1 == sections.len() { 0x91 } else { 0x90 };
            img[h + 1] = *p;
            img[h + 2] = 1;
            img[h + 32] = 0x88;
        }
        img
    }

    #[test]
    fn reads_el_torito() {
        let dir = std::env::temp_dir().join(format!("bcontain-eltorito-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let both = dir.join("hybrid.iso");
        std::fs::write(&both, eltorito(0, &[0xef])).unwrap();
        let b = iso_boot(&both).unwrap();
        assert!(b.bios && b.efi && !b.uefi_only());
        // Bazzite-style: the only entry is EFI.
        let efi = dir.join("efi.iso");
        std::fs::write(&efi, eltorito(0xef, &[])).unwrap();
        assert!(iso_boot(&efi).unwrap().uefi_only());
        let bios = dir.join("bios.iso");
        std::fs::write(&bios, eltorito(0, &[])).unwrap();
        assert_eq!(
            iso_boot(&bios),
            Some(IsoBoot {
                bios: true,
                efi: false
            })
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn recognises_windows_isos() {
        let w = tmp("winiso.iso", &iso_with_label("CCCOMA_X64FRE_EN-US_DV9"));
        assert_eq!(
            iso_volume_id(&w).as_deref(),
            Some("CCCOMA_X64FRE_EN-US_DV9")
        );
        assert!(is_windows_installer(&w));
        assert!(!is_windows_installer(&tmp(
            "other.iso",
            &iso_with_label("Ubuntu 24.04 LTS amd64")
        )));
        // A renamed ISO is still Windows by its label; a name alone works too.
        assert!(is_windows_installer(&tmp(
            "a.iso",
            &iso_with_label("CPBA_X64FRE_EN-US_DV5")
        )));
        assert!(is_windows_installer(&tmp(
            "Win11_24H2_English_x64.iso",
            &iso_with_label("X")
        )));
    }

    #[test]
    fn recognises_alphaos() {
        let dir = std::env::temp_dir().join(format!("bcontain-alphaos-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let at = |name: &str, bytes: &[u8]| {
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        };
        // By its label, whatever the file is called.
        assert!(is_alphaos_iso(&at(
            "renamed.iso",
            &iso_with_label("ALPHAOS")
        )));
        // By the release name, when the label is something else.
        assert!(is_alphaos_iso(&at(
            "alphaos-live-ubuntu24.04-amd64.iso",
            &iso_with_label("X")
        )));
        assert!(!is_alphaos_iso(&at(
            "ubuntu.iso",
            &iso_with_label("Ubuntu 24.04 LTS amd64")
        )));
        // The name alone is not enough: it must be an ISO.
        assert!(!is_alphaos_iso(&at("alphaos-notes.iso", b"not a disc")));
        assert!(!is_windows_installer(&at(
            "alphaos-x.iso",
            &iso_with_label("ALPHAOS")
        )));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_gpt() {
        let linux = [1u8; 16];
        let steam = gpt_disk(&[
            (ESP_GUID, "esp"),
            (linux, "efi-A"),
            (linux, "rootfs-A"),
            (linux, "home"),
        ]);
        let g = gpt_info(&tmp("gpt-steam", &steam)).unwrap();
        assert!(g.esp && !g.bios_boot && g.uefi_only());
        assert!(g.looks_like_steamos());
        assert_eq!(g.names, ["esp", "efi-A", "rootfs-A", "home"]);

        let both = gpt_disk(&[(BIOS_BOOT_GUID, "bios"), (ESP_GUID, "EFI"), (linux, "root")]);
        let g = gpt_info(&tmp("gpt-both", &both)).unwrap();
        assert!(!g.uefi_only() && !g.looks_like_steamos());

        assert!(gpt_info(&tmp("gpt-none", &[0u8; 8192])).is_none());
    }

    #[test]
    fn sniffs_formats() {
        let mut q = b"QFI\xfb".to_vec();
        q.resize(1024, 0);
        assert_eq!(
            sniff(&tmp("q", &q)).unwrap(),
            ImageKind::Disk(DiskFormat::Qcow2)
        );

        let mut v = b"vhdxfile".to_vec();
        v.resize(1024, 0);
        assert_eq!(
            sniff(&tmp("v", &v)).unwrap(),
            ImageKind::Disk(DiskFormat::Vhdx)
        );

        let mut iso = vec![0u8; 0x8800];
        iso[0x8001..0x8006].copy_from_slice(b"CD001");
        assert_eq!(
            sniff(&tmp("i", &iso)).unwrap(),
            ImageKind::Disk(DiskFormat::Iso)
        );

        let mut t = vec![0u8; 1024];
        t[257..262].copy_from_slice(b"ustar");
        assert_eq!(sniff(&tmp("t", &t)).unwrap(), ImageKind::OciArchive);

        assert_eq!(
            sniff(&tmp("r", &[0u8; 4096])).unwrap(),
            ImageKind::Disk(DiskFormat::Raw)
        );

        let mut bz = b"BZh91AY&SY".to_vec();
        bz.resize(1024, 0);
        assert_eq!(sniff(&tmp("bz", &bz)).unwrap(), ImageKind::Bzip2);
    }
}
