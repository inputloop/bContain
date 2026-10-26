// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Put a vCPU in 64-bit long mode and load a Linux `bzImage` per the x86
//! boot protocol (Documentation/arch/x86/boot.rst, "64-bit boot protocol").

use crate::layout::*;
use crate::memory::GuestMemory;
use crate::{Error, Result};

/// A segment register as both KVM and WHP want it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Segment {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    /// Descriptor type field (4 bits).
    pub type_: u8,
    /// 1 = code/data, 0 = system (TSS, LDT).
    pub s: bool,
    pub dpl: u8,
    pub present: bool,
    pub avl: bool,
    /// 64-bit code segment.
    pub l: bool,
    /// Default operand size (32-bit).
    pub db: bool,
    /// Limit is in 4 KiB units.
    pub g: bool,
}

impl Segment {
    /// The `Attributes` word WHP uses (type | S<<4 | DPL<<5 | P<<7 | AVL<<12 | L<<13 | DB<<14 | G<<15).
    pub fn attributes(&self) -> u16 {
        (self.type_ as u16 & 0xf)
            | (self.s as u16) << 4
            | (self.dpl as u16 & 3) << 5
            | (self.present as u16) << 7
            | (self.avl as u16) << 12
            | (self.l as u16) << 13
            | (self.db as u16) << 14
            | (self.g as u16) << 15
    }

    /// The limit in bytes, as KVM and WHP take it (the GDT stores 4 KiB units when `g`).
    pub fn byte_limit(&self) -> u32 {
        if self.g {
            (self.limit << 12) | 0xfff
        } else {
            self.limit
        }
    }

    /// Build the 8-byte GDT descriptor for this segment.
    pub fn descriptor(&self) -> u64 {
        let limit = self.limit as u64;
        let base = self.base;
        (limit & 0xffff)
            | (base & 0xff_ffff) << 16
            | (self.attributes() as u64 & 0xff) << 40
            | ((limit >> 16) & 0xf) << 48
            | ((self.attributes() as u64 >> 12) & 0xf) << 52
            | ((base >> 24) & 0xff) << 56
    }
}

/// Everything a platform needs to start the boot vCPU.
#[derive(Debug, Clone)]
pub struct BootState {
    pub rip: u64,
    pub rsi: u64,
    pub rsp: u64,
    pub rflags: u64,
    pub cr0: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub efer: u64,
    pub gdt_base: u64,
    pub gdt_limit: u16,
    pub cs: Segment,
    /// Used for DS, ES, FS, GS and SS.
    pub ds: Segment,
    pub tr: Segment,
    pub ldt: Segment,
}

const CR0_PE: u64 = 1;
const CR0_ET: u64 = 1 << 4;
const CR0_NE: u64 = 1 << 5;
const CR0_PG: u64 = 1 << 31;
const CR4_PAE: u64 = 1 << 5;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;

/// Write a GDT and identity page tables for the first 4 GiB, and return a
/// long-mode register state that starts executing at `entry`.
pub fn setup_long_mode(mem: &GuestMemory, entry: u64) -> Result<BootState> {
    let cs = Segment {
        base: 0,
        limit: 0xfffff,
        selector: 0x10,
        type_: 0xb, // execute/read, accessed
        s: true,
        present: true,
        l: true,
        g: true,
        ..Default::default()
    };
    let ds = Segment {
        base: 0,
        limit: 0xfffff,
        selector: 0x18,
        type_: 0x3, // read/write, accessed
        s: true,
        present: true,
        db: true,
        g: true,
        ..Default::default()
    };
    let tr = Segment {
        base: 0,
        limit: 0x67,
        selector: 0x20,
        type_: 0xb, // busy 64-bit TSS: required for VM entry in long mode
        present: true,
        ..Default::default()
    };
    let gdt: [u64; 5] = [0, 0, cs.descriptor(), ds.descriptor(), tr.descriptor()];
    for (i, d) in gdt.iter().enumerate() {
        mem.write_u64(GDT_ADDR + i as u64 * 8, *d)?;
    }

    // PML4[0] -> PDPT; PDPT[0..4] -> four page directories of 2 MiB pages.
    mem.write_u64(PML4_ADDR, PDPT_ADDR | 0x3)?;
    for i in 0..4u64 {
        let pd = PD_ADDR + i * 0x1000;
        mem.write_u64(PDPT_ADDR + i * 8, pd | 0x3)?;
        for j in 0..512u64 {
            let addr = (i * 512 + j) << 21;
            mem.write_u64(pd + j * 8, addr | 0x83)?; // present | writable | 2 MiB page
        }
    }

    Ok(BootState {
        rip: entry,
        rsi: 0,
        rsp: BOOT_STACK_ADDR,
        rflags: 0x2,
        cr0: CR0_PE | CR0_ET | CR0_NE | CR0_PG,
        cr3: PML4_ADDR,
        cr4: CR4_PAE,
        efer: EFER_LME | EFER_LMA,
        gdt_base: GDT_ADDR,
        gdt_limit: (gdt.len() * 8 - 1) as u16,
        cs,
        ds,
        tr,
        ldt: Segment {
            type_: 0x2,
            ..Default::default()
        }, // not present: unusable
    })
}

/// The parts of the bzImage setup header the loader needs.
#[derive(Debug, Clone)]
pub struct KernelHeader {
    pub setup_sects: usize,
    pub version: u16,
    pub xloadflags: u16,
    pub initrd_addr_max: u32,
    pub cmdline_size: u32,
    pub init_size: u32,
    /// Bytes of setup header to copy into `boot_params` (from 0x1f1).
    pub header_len: usize,
}

const HDR_START: usize = 0x1f1;
const XLF_KERNEL_64: u16 = 1;

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub fn parse_bzimage(image: &[u8]) -> Result<KernelHeader> {
    if image.len() < 0x270 || u16_at(image, 0x1fe) != 0xaa55 || &image[0x202..0x206] != b"HdrS" {
        return Err(Error::new(
            "not a Linux bzImage (missing boot signature / HdrS header)",
        ));
    }
    let version = u16_at(image, 0x206);
    if version < 0x020c {
        return Err(Error::new(format!(
            "kernel boot protocol {}.{} is too old; 2.12+ is needed for 64-bit entry",
            version >> 8,
            version & 0xff
        )));
    }
    let xloadflags = u16_at(image, 0x236);
    if xloadflags & XLF_KERNEL_64 == 0 {
        return Err(Error::new(
            "kernel has no 64-bit entry point (XLF_KERNEL_64 not set)",
        ));
    }
    let setup_sects = match image[0x1f1] {
        0 => 4,
        n => n as usize,
    };
    if (setup_sects + 1) * 512 >= image.len() {
        return Err(Error::new("bzImage is truncated"));
    }
    Ok(KernelHeader {
        setup_sects,
        version,
        xloadflags,
        initrd_addr_max: u32_at(image, 0x22c),
        cmdline_size: u32_at(image, 0x238),
        init_size: u32_at(image, 0x260),
        header_len: 0x202 + image[0x201] as usize - HDR_START,
    })
}

/// e820 entry types.
pub const E820_RAM: u32 = 1;
pub const E820_RESERVED: u32 = 2;

/// The memory map the guest sees.
pub fn e820_map(mem_size: u64) -> Vec<(u64, u64, u32)> {
    vec![
        (0, EBDA_START, E820_RAM),
        (EBDA_START, 0x10_0000 - EBDA_START, E820_RESERVED),
        (HIGH_RAM_START, mem_size - HIGH_RAM_START, E820_RAM),
    ]
}

/// Load `image` (+ `initrd`) and return the boot vCPU state.
pub fn load_linux(
    mem: &GuestMemory,
    image: &[u8],
    initrd: Option<&[u8]>,
    cmdline: &str,
) -> Result<BootState> {
    let hdr = parse_bzimage(image)?;
    let kernel = &image[(hdr.setup_sects + 1) * 512..];
    let need = KERNEL_LOAD_ADDR + hdr.init_size.max(kernel.len() as u32) as u64 + (16 << 20);
    if need > mem.size() {
        return Err(Error::new(format!(
            "guest needs at least {} MiB for this kernel",
            need >> 20
        )));
    }
    mem.write(KERNEL_LOAD_ADDR, kernel)?;

    // boot_params: start from the image's own setup header.
    let mut bp = vec![0u8; 4096];
    bp[HDR_START..HDR_START + hdr.header_len]
        .copy_from_slice(&image[HDR_START..HDR_START + hdr.header_len]);
    bp[0x1fa..0x1fc].copy_from_slice(&0xffffu16.to_le_bytes()); // vid_mode: "normal"
    bp[0x210] = 0xff; // type_of_loader: undefined loader
    bp[0x211] |= 0x01; // loadflags: LOADED_HIGH
    bp[0x214..0x218].copy_from_slice(&(KERNEL_LOAD_ADDR as u32).to_le_bytes()); // code32_start

    // Command line.
    let max = (hdr.cmdline_size as usize).min(CMDLINE_MAX - 1);
    if cmdline.len() > max {
        return Err(Error::new(format!(
            "kernel command line is longer than {max} bytes"
        )));
    }
    let mut c = cmdline.as_bytes().to_vec();
    c.push(0);
    mem.write(CMDLINE_ADDR, &c)?;
    bp[0x228..0x22c].copy_from_slice(&(CMDLINE_ADDR as u32).to_le_bytes());

    // Initrd: as high as allowed, page aligned, above the kernel's working area.
    if let Some(rd) = initrd {
        let ceiling = (hdr.initrd_addr_max as u64 + 1).min(mem.size());
        let addr = ceiling
            .checked_sub(rd.len() as u64)
            .map(|a| a & !0xfff)
            .unwrap_or(0);
        let kernel_end =
            KERNEL_LOAD_ADDR + (hdr.init_size as u64).max(kernel.len() as u64) + (16 << 20);
        if addr < kernel_end {
            return Err(Error::new(format!(
                "the initramfs ({} MiB) does not fit in guest memory with the kernel; raise the memory",
                rd.len() >> 20
            )));
        }
        mem.write(addr, rd)?;
        bp[0x218..0x21c].copy_from_slice(&(addr as u32).to_le_bytes());
        bp[0x21c..0x220].copy_from_slice(&(rd.len() as u32).to_le_bytes());
    }

    // e820 table at 0x2d0, count at 0x1e8.
    let map = e820_map(mem.size());
    bp[0x1e8] = map.len() as u8;
    for (i, (addr, size, kind)) in map.iter().enumerate() {
        let o = 0x2d0 + i * 20;
        bp[o..o + 8].copy_from_slice(&addr.to_le_bytes());
        bp[o + 8..o + 16].copy_from_slice(&size.to_le_bytes());
        bp[o + 16..o + 20].copy_from_slice(&kind.to_le_bytes());
    }
    mem.write(BOOT_PARAMS_ADDR, &bp)?;

    let mut state = setup_long_mode(mem, KERNEL_LOAD_ADDR + 0x200)?;
    state.rsi = BOOT_PARAMS_ADDR;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal image with a valid setup header and a 1 KiB "kernel".
    pub fn fake_bzimage() -> Vec<u8> {
        let mut img = vec![0u8; 5 * 512 + 1024];
        img[0x1f1] = 4; // setup_sects
        img[0x1fe..0x200].copy_from_slice(&0xaa55u16.to_le_bytes());
        img[0x201] = 0x6a; // jump offset: header ends at 0x26c
        img[0x202..0x206].copy_from_slice(b"HdrS");
        img[0x206..0x208].copy_from_slice(&0x020fu16.to_le_bytes());
        img[0x22c..0x230].copy_from_slice(&0x7fff_ffffu32.to_le_bytes());
        img[0x236..0x238].copy_from_slice(&XLF_KERNEL_64.to_le_bytes());
        img[0x238..0x23c].copy_from_slice(&2048u32.to_le_bytes());
        img[0x260..0x264].copy_from_slice(&(8u32 << 20).to_le_bytes());
        img[5 * 512] = 0xf4; // first kernel byte
        img
    }

    #[test]
    fn descriptors_match_known_values() {
        let m = GuestMemory::new(64 << 20).unwrap();
        let s = setup_long_mode(&m, 0x1234).unwrap();
        // The classic flat 64-bit code and data descriptors.
        assert_eq!(s.cs.descriptor(), 0x00af_9b00_0000_ffff);
        assert_eq!(s.ds.descriptor(), 0x00cf_9300_0000_ffff);
        assert_eq!(m.read_u64(GDT_ADDR + 0x10).unwrap(), 0x00af_9b00_0000_ffff);
        assert_eq!(s.cs.attributes(), 0xa09b);
        assert_eq!(s.tr.attributes(), 0x008b);
        assert_eq!(s.cs.byte_limit(), 0xffff_ffff);
        assert_eq!(s.tr.byte_limit(), 0x67);
    }

    #[test]
    fn page_tables_identity_map_4g() {
        let m = GuestMemory::new(64 << 20).unwrap();
        setup_long_mode(&m, 0).unwrap();
        let translate = |va: u64| -> u64 {
            let pml4e = m.read_u64(PML4_ADDR + ((va >> 39) & 0x1ff) * 8).unwrap();
            let pdpte = m
                .read_u64((pml4e & !0xfff) + ((va >> 30) & 0x1ff) * 8)
                .unwrap();
            let pde = m
                .read_u64((pdpte & !0xfff) + ((va >> 21) & 0x1ff) * 8)
                .unwrap();
            assert_eq!(pde & 0x81, 0x81, "present 2 MiB page");
            (pde & !0x1f_ffff & 0x000f_ffff_ffff_f000) | (va & 0x1f_ffff)
        };
        for va in [0u64, 0x10_0000, 0x7fff_f123, 0xfec0_0010, 0xffff_ffff] {
            assert_eq!(translate(va), va);
        }
    }

    #[test]
    fn loads_bzimage() {
        let m = GuestMemory::new(128 << 20).unwrap();
        let rd = vec![7u8; 4096 * 3];
        let st = load_linux(&m, &fake_bzimage(), Some(&rd), "console=ttyS0").unwrap();
        assert_eq!(st.rip, KERNEL_LOAD_ADDR + 0x200);
        assert_eq!(st.rsi, BOOT_PARAMS_ADDR);
        let mut bp = vec![0u8; 4096];
        m.read(BOOT_PARAMS_ADDR, &mut bp).unwrap();
        assert_eq!(&bp[0x202..0x206], b"HdrS");
        assert_eq!(bp[0x210], 0xff);
        assert_eq!(u32_at(&bp, 0x228) as u64, CMDLINE_ADDR);
        let rd_addr = u32_at(&bp, 0x218) as u64;
        assert_eq!(rd_addr % 4096, 0);
        assert_eq!(u32_at(&bp, 0x21c), rd.len() as u32);
        assert!(rd_addr + rd.len() as u64 <= m.size());
        let mut back = vec![0u8; rd.len()];
        m.read(rd_addr, &mut back).unwrap();
        assert_eq!(back, rd);
        assert_eq!(bp[0x1e8], 3);
        let mut first = [0u8; 1];
        m.read(KERNEL_LOAD_ADDR, &mut first).unwrap();
        assert_eq!(first[0], 0xf4);
        let mut cl = vec![0u8; 14];
        m.read(CMDLINE_ADDR, &mut cl).unwrap();
        assert_eq!(&cl, b"console=ttyS0\0");
    }

    #[test]
    fn rejects_bad_images() {
        assert!(parse_bzimage(&[0u8; 4096]).is_err());
        let mut img = fake_bzimage();
        img[0x236] = 0;
        assert!(parse_bzimage(&img)
            .unwrap_err()
            .to_string()
            .contains("64-bit"));
        let mut img = fake_bzimage();
        img[0x206] = 0x0b;
        assert!(parse_bzimage(&img)
            .unwrap_err()
            .to_string()
            .contains("too old"));
    }
}
