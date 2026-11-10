// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! What a virtual disk is made of.
//!
//! Either one raw image file, or a *layout*: a small text file listing
//! pieces that are laid end to end to form the disk. A layout lets a
//! partitioned disk be assembled from existing partition images without
//! copying them: the partition table is a couple of small files and each
//! partition is the image it already is.
//!
//! A layout file starts with the line `bvmm-disk-layout 1`. Every other
//! non-empty line that does not start with `#` is one piece:
//!
//! ```text
//! <bytes> <ro|rw> <path>
//! <bytes> zero
//! ```
//!
//! `bytes` is the piece's length and must be a multiple of 512. A file
//! shorter than its piece reads as zeros past its end. `zero` is a gap that
//! reads as zeros and ignores writes. Relative paths are relative to the
//! layout file. The path is the rest of the line, so it may contain spaces.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::{Error, Result};

const LAYOUT_MAGIC: &str = "bvmm-disk-layout 1";

enum Source {
    Zero,
    File {
        file: File,
        len: u64,
        read_only: bool,
    },
}

struct Piece {
    start: u64,
    len: u64,
    source: Source,
}

pub struct Disk {
    pieces: Vec<Piece>,
    len: u64,
    read_only: bool,
}

fn io_err(what: &str, path: &Path, e: std::io::Error) -> Error {
    Error::new(format!("{what} {}: {e}", path.display()))
}

fn open_file(path: &Path, read_only: bool) -> Result<(File, u64)> {
    let file = OpenOptions::new()
        .read(true)
        .write(!read_only)
        .open(path)
        .map_err(|e| io_err("opening disk", path, e))?;
    let len = file
        .metadata()
        .map_err(|e| io_err("reading the size of", path, e))?
        .len();
    Ok((file, len))
}

impl Disk {
    /// Open `path` as a raw image, or as a layout if it starts with the
    /// layout magic. `read_only` makes the whole disk read-only.
    pub fn open(path: &Path, read_only: bool) -> Result<Disk> {
        let mut head = [0u8; LAYOUT_MAGIC.len()];
        let is_layout = File::open(path)
            .and_then(|mut f| f.read_exact(&mut head))
            .is_ok()
            && head == LAYOUT_MAGIC.as_bytes();
        if is_layout {
            let text = std::fs::read_to_string(path).map_err(|e| io_err("reading", path, e))?;
            return Disk::from_layout(&text, path.parent().unwrap_or(Path::new(".")), read_only);
        }
        let (file, len) = open_file(path, read_only)?;
        Ok(Disk {
            pieces: vec![Piece {
                start: 0,
                len,
                source: Source::File {
                    file,
                    len,
                    read_only,
                },
            }],
            len,
            read_only,
        })
    }

    fn from_layout(text: &str, base: &Path, read_only: bool) -> Result<Disk> {
        let mut pieces = Vec::new();
        let mut at = 0u64;
        for (n, line) in text.lines().enumerate().skip(1) {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bad = |why: &str| Error::new(format!("disk layout line {}: {why}", n + 1));
            let (len, rest) = line
                .split_once(' ')
                .ok_or_else(|| bad("expected a length"))?;
            let len: u64 = len.parse().map_err(|_| bad("the length is not a number"))?;
            if len == 0 || !len.is_multiple_of(512) {
                return Err(bad("the length must be a positive multiple of 512"));
            }
            let rest = rest.trim();
            let source = if rest == "zero" {
                Source::Zero
            } else {
                let (mode, path) = rest
                    .split_once(' ')
                    .ok_or_else(|| bad("expected `ro <path>`, `rw <path>` or `zero`"))?;
                let piece_ro = match mode {
                    "ro" => true,
                    "rw" => false,
                    _ => return Err(bad("the mode must be `ro` or `rw`")),
                };
                let (file, file_len) = open_file(&base.join(path.trim()), piece_ro || read_only)?;
                Source::File {
                    file,
                    len: file_len,
                    read_only: piece_ro || read_only,
                }
            };
            pieces.push(Piece {
                start: at,
                len,
                source,
            });
            at = at
                .checked_add(len)
                .ok_or_else(|| bad("the disk is too large"))?;
        }
        if pieces.is_empty() {
            return Err(Error::new("disk layout has no pieces"));
        }
        Ok(Disk {
            pieces,
            len: at,
            read_only,
        })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// True if no part of the disk can be written.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Call `f(piece, offset in piece, range of buf)` for each piece that
    /// `offset..offset+len` touches.
    fn each(
        &mut self,
        offset: u64,
        len: usize,
        mut f: impl FnMut(&mut Piece, u64, std::ops::Range<usize>) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let end = offset
            .checked_add(len as u64)
            .filter(|e| *e <= self.len)
            .ok_or_else(|| std::io::Error::other("access past the end of the disk"))?;
        let first = self.pieces.partition_point(|p| p.start + p.len <= offset);
        let mut at = offset;
        for piece in &mut self.pieces[first..] {
            if at >= end {
                break;
            }
            let within = at - piece.start;
            let take = (piece.len - within).min(end - at);
            let from = (at - offset) as usize;
            f(piece, within, from..from + take as usize)?;
            at += take;
        }
        Ok(())
    }

    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let len = buf.len();
        self.each(offset, len, |piece, within, range| {
            let out = &mut buf[range];
            out.fill(0);
            if let Source::File { file, len, .. } = &mut piece.source {
                // Past the end of a short file the piece reads as zeros.
                let have = len.saturating_sub(within).min(out.len() as u64) as usize;
                if have > 0 {
                    file.seek(SeekFrom::Start(within))?;
                    file.read_exact(&mut out[..have])?;
                }
            }
            Ok(())
        })
    }

    pub fn write_at(&mut self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        self.each(offset, buf.len(), |piece, within, range| {
            match &mut piece.source {
                Source::Zero => Ok(()),
                Source::File {
                    read_only: true, ..
                } => Err(std::io::Error::other("this part of the disk is read-only")),
                Source::File { file, len, .. } => {
                    file.seek(SeekFrom::Start(within))?;
                    file.write_all(&buf[range.clone()])?;
                    *len = (*len).max(within + range.len() as u64);
                    Ok(())
                }
            }
        })
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        for piece in &mut self.pieces {
            if let Source::File {
                file,
                read_only: false,
                ..
            } = &mut piece.source
            {
                file.sync_data()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bvmm-disk-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_layout_lays_pieces_end_to_end() {
        let d = dir("layout");
        std::fs::write(d.join("a.img"), [0xaa; 1024]).unwrap();
        std::fs::write(d.join("short one.img"), [0xbb; 100]).unwrap();
        std::fs::write(d.join("ro.img"), [0xcc; 512]).unwrap();
        let layout = d.join("disk.layout");
        std::fs::write(
            &layout,
            "bvmm-disk-layout 1\n# a comment\n1024 rw a.img\n512 zero\n\n1024 rw short one.img\n512 ro ro.img\n",
        )
        .unwrap();
        let mut disk = Disk::open(&layout, false).unwrap();
        assert_eq!(disk.len(), 3072);
        assert!(!disk.read_only());

        // One read across all four pieces.
        let mut all = vec![1u8; 3072];
        disk.read_at(0, &mut all).unwrap();
        assert!(all[..1024].iter().all(|b| *b == 0xaa));
        assert!(all[1024..1536].iter().all(|b| *b == 0), "the gap");
        assert!(all[1536..1636].iter().all(|b| *b == 0xbb));
        assert!(all[1636..2560].iter().all(|b| *b == 0), "past a short file");
        assert!(all[2560..].iter().all(|b| *b == 0xcc));

        // A write across the first piece, the gap and the short file.
        disk.write_at(1000, &[0x11; 700]).unwrap();
        disk.flush().unwrap();
        let mut back = vec![0u8; 700];
        disk.read_at(1000, &mut back).unwrap();
        assert!(back[..24].iter().all(|b| *b == 0x11));
        assert!(
            back[24..536].iter().all(|b| *b == 0),
            "the gap ignores writes"
        );
        assert!(back[536..].iter().all(|b| *b == 0x11));
        assert_eq!(std::fs::read(d.join("a.img")).unwrap()[1000], 0x11);
        assert_eq!(std::fs::read(d.join("short one.img")).unwrap().len(), 164);

        // Read-only pieces refuse, and so does anything past the end.
        assert!(disk.write_at(2560, &[0; 512]).is_err());
        assert!(disk.read_at(3072, &mut [0; 1]).is_err());
        assert!(disk.read_at(u64::MAX, &mut [0; 2]).is_err());
        assert_eq!(std::fs::read(d.join("ro.img")).unwrap(), [0xcc; 512]);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn bad_layouts_are_refused_with_the_line_number() {
        let d = dir("bad");
        std::fs::write(d.join("a.img"), [0; 512]).unwrap();
        let check = |body: &str, want: &str| {
            let p = d.join("l");
            std::fs::write(&p, format!("bvmm-disk-layout 1\n{body}")).unwrap();
            let e = Disk::open(&p, false).err().expect(body).to_string();
            assert!(e.contains(want), "{body:?}: {e}");
        };
        check("", "no pieces");
        check("100 rw a.img\n", "line 2");
        check("512 rw a.img\n512 maybe a.img\n", "line 3");
        check("512 rw missing.img\n", "missing.img");
        check("many zero\n", "not a number");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_plain_file_is_a_one_piece_disk() {
        let d = dir("raw");
        let p = d.join("raw.img");
        std::fs::write(&p, [7u8; 2048]).unwrap();
        let mut disk = Disk::open(&p, true).unwrap();
        assert_eq!(disk.len(), 2048);
        assert!(disk.read_only());
        let mut b = [0u8; 4];
        disk.read_at(2044, &mut b).unwrap();
        assert_eq!(b, [7; 4]);
        std::fs::remove_dir_all(d).unwrap();
    }
}
