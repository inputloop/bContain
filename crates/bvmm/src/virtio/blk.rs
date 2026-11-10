// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! virtio-blk (Virtio 1.2, section 5.2) backed by a raw image file, or by
//! several files laid end to end (see [`crate::disk`]).
//! Requests are served on the thread that delivered the kick.

use std::path::Path;

use super::queue::{Chain, Queue};
use super::{VirtioDevice, TYPE_BLOCK};
use crate::disk::Disk;
use crate::memory::GuestMemory;
use crate::Result;

const SECTOR: u64 = 512;
const F_RO: u64 = 1 << 5;
const F_FLUSH: u64 = 1 << 9;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

/// The largest single buffer served; a guest asking for more gets an error
/// instead of making the host allocate without bound.
const MAX_BUFFER: u32 = 8 << 20;

pub struct Block {
    disk: Disk,
    sectors: u64,
    read_only: bool,
    id: [u8; 20],
}

impl Block {
    pub fn open(path: &Path, read_only: bool) -> Result<Block> {
        let disk = Disk::open(path, read_only)?;
        let mut id = [0u8; 20];
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        for (d, s) in id.iter_mut().zip(name.unwrap_or_default().bytes()) {
            *d = s;
        }
        Ok(Block {
            sectors: disk.len() / SECTOR,
            read_only: read_only || disk.read_only(),
            disk,
            id,
        })
    }

    /// Serve one request. Returns the bytes written into guest buffers.
    fn serve(&mut self, chain: &Chain, mem: &GuestMemory) -> u32 {
        let n = chain.descs.len();
        // Header, then data, then one writable status byte.
        if n < 2 {
            return 0;
        }
        let (head, status) = (chain.descs[0], chain.descs[n - 1]);
        if head.writable || head.len < 16 || !status.writable || status.len < 1 {
            return 0;
        }
        let data = &chain.descs[1..n - 1];
        let mut written = 0u32;
        let result = (|| -> std::result::Result<u8, ()> {
            let kind = mem.read_u32(head.addr).map_err(|_| ())?;
            let sector = mem.read_u64(head.addr + 8).map_err(|_| ())?;
            let total: u64 = data.iter().map(|d| d.len as u64).sum();
            let in_range = sector
                .checked_mul(SECTOR)
                .and_then(|o| o.checked_add(total))
                .is_some_and(|end| end <= self.sectors * SECTOR);
            match kind {
                T_IN | T_OUT => {
                    if !in_range || !total.is_multiple_of(SECTOR) {
                        return Ok(S_IOERR);
                    }
                    if kind == T_OUT && self.read_only {
                        return Ok(S_IOERR);
                    }
                    let mut at = sector * SECTOR;
                    for d in data {
                        if d.len > MAX_BUFFER || d.writable != (kind == T_IN) {
                            return Ok(S_IOERR);
                        }
                        let mut buf = vec![0u8; d.len as usize];
                        if kind == T_IN {
                            self.disk.read_at(at, &mut buf).map_err(|_| ())?;
                            mem.write(d.addr, &buf).map_err(|_| ())?;
                            written += d.len;
                        } else {
                            mem.read(d.addr, &mut buf).map_err(|_| ())?;
                            self.disk.write_at(at, &buf).map_err(|_| ())?;
                        }
                        at += d.len as u64;
                    }
                    Ok(S_OK)
                }
                T_FLUSH => {
                    self.disk.flush().map_err(|_| ())?;
                    Ok(S_OK)
                }
                T_GET_ID => {
                    let Some(d) = data.first().filter(|d| d.writable && d.len >= 20) else {
                        return Ok(S_IOERR);
                    };
                    mem.write(d.addr, &self.id).map_err(|_| ())?;
                    written += 20;
                    Ok(S_OK)
                }
                _ => Ok(S_UNSUPP),
            }
        })();
        let code = result.unwrap_or(S_IOERR);
        if mem.write(status.addr, &[code]).is_ok() {
            written += 1;
        }
        written
    }
}

impl VirtioDevice for Block {
    fn device_type(&self) -> u32 {
        TYPE_BLOCK
    }

    fn features(&self) -> u64 {
        F_FLUSH | if self.read_only { F_RO } else { 0 }
    }

    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![256]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // struct virtio_blk_config starts with the capacity in 512-byte sectors.
        let cfg = self.sectors.to_le_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn notify(&mut self, _index: usize, queue: &mut Queue, mem: &GuestMemory) -> bool {
        let mut any = false;
        while let Ok(Some(chain)) = queue.pop(mem) {
            let written = self.serve(&chain, mem);
            if queue.push_used(mem, chain.head, written).is_err() {
                break;
            }
            any = true;
        }
        any
    }
}

#[cfg(test)]
mod tests {
    use super::super::queue::tests::{publish, put_desc, queue, USED};
    use super::*;

    const NEXT: u16 = 1;
    const WRITE: u16 = 2;
    const HDR: u64 = 0x10000;
    const DATA: u64 = 0x20000;
    const STATUS: u64 = 0x30000;

    fn disk(name: &str, sectors: usize) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("bvmm-blk-{}-{name}", std::process::id()));
        let mut bytes = vec![0u8; sectors * 512];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i / 512) as u8;
        }
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn request(mem: &GuestMemory, kind: u32, sector: u64, len: u32, n: u16) {
        mem.write_u32(HDR, kind).unwrap();
        mem.write_u32(HDR + 4, 0).unwrap();
        mem.write_u64(HDR + 8, sector).unwrap();
        mem.write(STATUS, &[0xff]).unwrap();
        put_desc(mem, 0, HDR, 16, NEXT, 1);
        let data_flags = NEXT | if kind == T_OUT { 0 } else { WRITE };
        put_desc(mem, 1, DATA, len, data_flags, 2);
        put_desc(mem, 2, STATUS, 1, WRITE, 0);
        publish(mem, 8, n, 0);
    }

    fn status(mem: &GuestMemory) -> u8 {
        let mut s = [0u8; 1];
        mem.read(STATUS, &mut s).unwrap();
        s[0]
    }

    #[test]
    fn reads_writes_and_reports_capacity() {
        let path = disk("rw", 16);
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut q = queue(8);
        let mut blk = Block::open(&path, false).unwrap();
        let mut cap = [0u8; 8];
        blk.read_config(0, &mut cap);
        assert_eq!(u64::from_le_bytes(cap), 16);

        request(&mem, T_IN, 3, 1024, 0);
        assert!(blk.notify(0, &mut q, &mem));
        assert_eq!(status(&mem), S_OK);
        let mut got = [0u8; 1024];
        mem.read(DATA, &mut got).unwrap();
        assert!(got[..512].iter().all(|b| *b == 3) && got[512..].iter().all(|b| *b == 4));
        assert_eq!(mem.read_u32(USED + 8).unwrap(), 1025, "data plus status");

        mem.write(DATA, &[0xab; 512]).unwrap();
        request(&mem, T_OUT, 7, 512, 1);
        assert!(blk.notify(0, &mut q, &mem));
        assert_eq!(status(&mem), S_OK);
        request(&mem, T_FLUSH, 0, 0, 2);
        assert!(blk.notify(0, &mut q, &mem));
        assert_eq!(status(&mem), S_OK);
        drop(blk);
        let on_disk = std::fs::read(&path).unwrap();
        assert!(on_disk[7 * 512..8 * 512].iter().all(|b| *b == 0xab));
        assert_eq!(on_disk[6 * 512], 6, "neighbouring sectors untouched");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn refuses_what_it_should() {
        let path = disk("ro", 4);
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut q = queue(8);
        let mut blk = Block::open(&path, true).unwrap();
        assert_ne!(blk.features() & F_RO, 0);

        mem.write(DATA, &[1; 512]).unwrap();
        request(&mem, T_OUT, 0, 512, 0);
        blk.notify(0, &mut q, &mem);
        assert_eq!(status(&mem), S_IOERR, "write to a read-only disk");

        request(&mem, T_IN, 4, 512, 1);
        blk.notify(0, &mut q, &mem);
        assert_eq!(status(&mem), S_IOERR, "read past the end");

        request(&mem, T_IN, u64::MAX, 512, 2);
        blk.notify(0, &mut q, &mem);
        assert_eq!(status(&mem), S_IOERR, "sector overflow");

        request(&mem, 99, 0, 512, 3);
        blk.notify(0, &mut q, &mem);
        assert_eq!(status(&mem), S_UNSUPP);

        request(&mem, T_GET_ID, 0, 20, 4);
        blk.notify(0, &mut q, &mem);
        assert_eq!(status(&mem), S_OK);
        let mut id = [0u8; 8];
        mem.read(DATA, &mut id).unwrap();
        assert_eq!(&id, b"bvmm-blk");
        assert_eq!(std::fs::read(&path).unwrap()[0], 0, "disk unchanged");
        std::fs::remove_file(path).unwrap();
    }
}
