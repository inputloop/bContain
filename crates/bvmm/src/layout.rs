// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! The guest-physical address map. Everything the loader places lives below
//! 1 MiB except the kernel and initrd.

/// The Linux 64-bit boot protocol expects code at selector 0x10, data at 0x18.
pub const GDT_ADDR: u64 = 0x500;
/// `struct boot_params` ("zero page").
pub const BOOT_PARAMS_ADDR: u64 = 0x7000;
/// Boot stack, growing down below the zero page.
pub const BOOT_STACK_ADDR: u64 = 0x6ff0;
/// Identity-mapped page tables covering the first 4 GiB with 2 MiB pages.
pub const PML4_ADDR: u64 = 0x9000;
pub const PDPT_ADDR: u64 = 0xa000;
pub const PD_ADDR: u64 = 0xb000; // four tables: 0xb000..0xf000
pub const CMDLINE_ADDR: u64 = 0x20000;
pub const CMDLINE_MAX: usize = 0x10000;
/// Self-test code runs here.
pub const SELFTEST_CODE_ADDR: u64 = 0x10000;
/// MP floating pointer + configuration table, in the area Linux scans
/// (the last KiB below 640 KiB).
pub const MPTABLE_ADDR: u64 = 0x9fc00;
pub const EBDA_START: u64 = 0x9fc00;
pub const HIGH_RAM_START: u64 = 0x10_0000;
/// Kernel is loaded at 1 MiB; the decompressor relocates itself as needed.
pub const KERNEL_LOAD_ADDR: u64 = HIGH_RAM_START;
/// Guest RAM ends below this; the 32-bit MMIO hole starts here.
pub const MMIO_HOLE_START: u64 = 0xc000_0000;
pub const IOAPIC_ADDR: u64 = 0xfec0_0000;
pub const LAPIC_ADDR: u64 = 0xfee0_0000;
