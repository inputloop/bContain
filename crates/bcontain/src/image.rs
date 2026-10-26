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
