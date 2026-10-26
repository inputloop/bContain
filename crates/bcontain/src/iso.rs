// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! A minimal ISO 9660 writer: one directory, a few small files, with
//! level-2 and Joliet names. Enough for the answer-file CD Windows Setup
//! reads (`autounattend.xml`), without any external tool.

use std::path::Path;

use anyhow::{bail, Result};

const SECTOR: usize = 2048;

fn both16(v: u16) -> [u8; 4] {
    let mut b = [0u8; 4];
    b[..2].copy_from_slice(&v.to_le_bytes());
    b[2..].copy_from_slice(&v.to_be_bytes());
    b
}

fn both32(v: u32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&v.to_le_bytes());
    b[4..].copy_from_slice(&v.to_be_bytes());
    b
}

/// A directory record. `name` is the raw identifier bytes.
fn dir_record(name: &[u8], lba: u32, size: u32, is_dir: bool) -> Vec<u8> {
    let mut r = vec![0u8; 33];
    r[2..10].copy_from_slice(&both32(lba));
    r[10..18].copy_from_slice(&both32(size));
    r[18..25].copy_from_slice(&[126, 1, 1, 0, 0, 0, 0]); // 2026-01-01, UTC
    r[25] = if is_dir { 0x02 } else { 0 };
    r[28..32].copy_from_slice(&both16(1));
    r[32] = name.len() as u8;
    r.extend_from_slice(name);
    if r.len() % 2 == 1 {
        r.push(0);
    }
    r[0] = r.len() as u8;
    r
}

fn put_str(dst: &mut [u8], s: &str) {
    dst.fill(b' ');
    let n = s.len().min(dst.len());
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
}

fn put_ucs2(dst: &mut [u8], s: &str) {
    for c in dst.chunks_exact_mut(2) {
        c.copy_from_slice(&[0, b' ']);
    }
    for (c, u) in dst.chunks_exact_mut(2).zip(s.encode_utf16()) {
        c.copy_from_slice(&u.to_be_bytes());
    }
}

fn ucs2(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
}

/// Write a root directory (".", "..", then the files) at `sector`.
fn write_root(
    img: &mut [u8],
    sector: u32,
    joliet: bool,
    files: &[(&str, &[u8])],
    extents: &[u32],
) -> Result<()> {
    let mut d = Vec::new();
    d.extend(dir_record(&[0], sector, SECTOR as u32, true));
    d.extend(dir_record(&[1], sector, SECTOR as u32, true));
    for ((name, data), lba) in files.iter().zip(extents) {
        let id = if joliet {
            ucs2(&format!("{name};1"))
        } else {
            format!("{};1", name.to_uppercase()).into_bytes()
        };
        d.extend(dir_record(&id, *lba, data.len() as u32, false));
    }
    if d.len() > SECTOR {
        bail!("too many files for one directory sector");
    }
    let at = sector as usize * SECTOR;
    img[at..at + d.len()].copy_from_slice(&d);
    Ok(())
}

/// Fill a primary (or Joliet supplementary) volume descriptor.
fn descriptor(v: &mut [u8], joliet: bool, volume_id: &str, total: u32, roots: (u32, u32)) {
    v[0] = if joliet { 2 } else { 1 };
    v[1..6].copy_from_slice(b"CD001");
    v[6] = 1;
    if joliet {
        put_ucs2(&mut v[8..40], "");
        put_ucs2(&mut v[40..72], volume_id);
        v[88..91].copy_from_slice(b"%/E"); // UCS-2 level 3
    } else {
        put_str(&mut v[8..40], "");
        put_str(&mut v[40..72], volume_id);
    }
    v[80..88].copy_from_slice(&both32(total));
    v[120..124].copy_from_slice(&both16(1));
    v[124..128].copy_from_slice(&both16(1));
    v[128..132].copy_from_slice(&both16(SECTOR as u16));
    v[132..140].copy_from_slice(&both32(10));
    let (l, m, root) = if joliet {
        (21u32, 22u32, roots.1)
    } else {
        (19, 20, roots.0)
    };
    v[140..144].copy_from_slice(&l.to_le_bytes());
    v[148..152].copy_from_slice(&m.to_be_bytes());
    let r = dir_record(&[0], root, SECTOR as u32, true);
    v[156..156 + r.len()].copy_from_slice(&r);
    for (from, to) in [
        (190, 318),
        (318, 446),
        (446, 574),
        (574, 702),
        (702, 739),
        (739, 776),
        (776, 813),
    ] {
        if joliet {
            put_ucs2(&mut v[from..to - (to - from) % 2], "");
        } else {
            put_str(&mut v[from..to], "");
        }
    }
    for at in [813, 830, 847, 864] {
        v[at..at + 16].copy_from_slice(b"0000000000000000");
        v[at + 16] = 0;
    }
    v[881] = 1;
}

/// Build an ISO image holding `files` (name, contents) in its root.
pub fn build(volume_id: &str, files: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    if files.is_empty() || files.len() > 20 {
        bail!("this writer handles 1-20 files");
    }
    // Sector map: 16 system, 16 PVD, 17 Joliet SVD, 18 terminator,
    // 19-22 path tables (ISO L/M, Joliet L/M), 23 ISO root, 24 Joliet root, 25.. data.
    let (pvd, svd, term) = (16, 17, 18);
    let (iso_root, joliet_root) = (23u32, 24u32);
    let mut next = 25u32;
    let mut extents = Vec::new();
    for (_, data) in files {
        extents.push(next);
        next += data.len().div_ceil(SECTOR).max(1) as u32;
    }
    let total = next as usize;
    let mut img = vec![0u8; total * SECTOR];

    write_root(&mut img, iso_root, false, files, &extents)?;
    write_root(&mut img, joliet_root, true, files, &extents)?;

    for ((_, data), lba) in files.iter().zip(&extents) {
        let at = *lba as usize * SECTOR;
        img[at..at + data.len()].copy_from_slice(data);
    }

    // Path tables: just the root (10 bytes each).
    let path_table = |root: u32, big_endian: bool| -> [u8; 10] {
        let mut p = [0u8; 10];
        p[0] = 1;
        p[2..6].copy_from_slice(&if big_endian {
            root.to_be_bytes()
        } else {
            root.to_le_bytes()
        });
        p[6..8].copy_from_slice(&if big_endian {
            1u16.to_be_bytes()
        } else {
            1u16.to_le_bytes()
        });
        p
    };
    for (sector, root, be) in [
        (19, iso_root, false),
        (20, iso_root, true),
        (21, joliet_root, false),
        (22, joliet_root, true),
    ] {
        let at = sector * SECTOR;
        img[at..at + 10].copy_from_slice(&path_table(root, be));
    }

    let roots = (iso_root, joliet_root);
    descriptor(
        &mut img[pvd * SECTOR..(pvd + 1) * SECTOR],
        false,
        volume_id,
        total as u32,
        roots,
    );
    descriptor(
        &mut img[svd * SECTOR..(svd + 1) * SECTOR],
        true,
        volume_id,
        total as u32,
        roots,
    );
    let t = &mut img[term * SECTOR..(term + 1) * SECTOR];
    t[0] = 255;
    t[1..6].copy_from_slice(b"CD001");
    t[6] = 1;
    Ok(img)
}

pub fn write(path: &Path, volume_id: &str, files: &[(&str, &[u8])]) -> Result<()> {
    std::fs::write(path, build(volume_id, files)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let img = build("BCONTAIN", &[("autounattend.xml", b"<unattend/>")]).unwrap();
        assert_eq!(img.len(), 26 * SECTOR);
        assert_eq!(&img[16 * SECTOR + 1..16 * SECTOR + 6], b"CD001");
        assert_eq!(img[17 * SECTOR], 2);
        assert_eq!(img[18 * SECTOR], 255);
        assert_eq!(&img[25 * SECTOR..25 * SECTOR + 11], b"<unattend/>");
        // The ISO root lists the level-2 name.
        let root = &img[23 * SECTOR..24 * SECTOR];
        assert!(root.windows(18).any(|w| w == b"AUTOUNATTEND.XML;1"));
    }
}
