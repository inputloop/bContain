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
    }
}
