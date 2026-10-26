<!-- SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors -->
<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# bContain's own hypervisor (`crates/bvmm`)

bvmm is bContain's own virtual machine monitor. It boots a Linux kernel
straight into 64-bit mode on the host's hardware virtualization: **KVM** on
Linux and **Windows Hypervisor Platform (WHP)** on Windows. Today it runs
bContain's **container images**. Disk images and ISOs still run on QEMU.

```sh
bcontain hypervisor-test                                   # step-by-step check of this machine
bcontain run --engine native alpine:3.20 -- uname -a      # a container on bvmm
bcontain create web nginx:alpine --engine native          # a persistent one
```

In the app, the **This machine** panel has a **Test it** button, and the
**Add a machine** form offers "Run on bContain's own hypervisor" for
container images.

## What the guest gets

| Piece | Where | Notes |
|---|---|---|
| RAM (64 MiB – 3 GiB) | `memory.rs` | one region below the 32-bit MMIO hole |
| 64-bit entry | `boot.rs` | GDT, identity page tables for 4 GiB, Linux boot protocol 2.12+ (zero page, e820, command line, initrd) |
| MP table | `mptable.rs` | lets the kernel find the CPU and I/O APIC without ACPI |
| 16550 UART on COM1 | `devices/serial.rs` | the console; passes Linux's loopback probe; IRQ 4 |
| CMOS clock | `devices/cmos.rs` | correct date at boot |
| I/O APIC | `devices/ioapic.rs` | WHP only (KVM has one in the kernel) |
| Reset | `devices/mod.rs` | keyboard controller `0xfe` / port `0xcf9`; container init reboots to exit |
| KVM backend | `platform/kvm.rs` | rust-vmm `kvm-ioctls`; in-kernel PIC, I/O APIC, local APIC and PIT |
| WHP backend | `platform/whp.rs` | DLLs loaded at run time; local APIC emulated by WHP; Microsoft's instruction emulator for port I/O and MMIO; Hyper-V enlightenments so the kernel reads its timer frequencies instead of calibrating a PIT |

Not yet: more than one vCPU, a network card, disks (virtio-mmio is the
plan), clean shutdown via ACPI.

## How it is checked

Nothing in this sandbox has hardware virtualization, so every layer that can
be checked without it is:

- **Unit tests** cover the boot loader (header parsing, zero page, initrd
  placement, page tables that identity-map 4 GiB, flat GDT descriptors),
  the MP table checksums and entry walk, the UART (Linux's loopback probe,
  divisor latch, interrupt edges), the I/O APIC (routing, masking,
  level-triggered EOI), the CMOS clock, and interrupt routing through the
  device bus.
- **The WHP FFI layout** is asserted twice: Rust compile-time `size_of` and
  `offset_of` checks, and `scripts/check-whp-layout.sh`, which compiles the
  same numbers against Microsoft's `winhvplatformdefs.h` / `winhvemulation.h`
  with MinGW. That includes the Hyper-V feature mask, read back from the
  compiled C bitfield.
- **On real hardware**, `bcontain hypervisor-test` runs a 40-byte guest
  program in long mode. It prints `OK` through port I/O, reads the I/O APIC
  version over MMIO, prints it, and resets the machine. If any step fails,
  the test says which one.

## Confirmed on real hardware

- **2026-10-26, reference laptop** (i9-12900H, 31.8 GiB, RTX 3080 Ti Laptop,
  Windows Hypervisor Platform, bContain's single-file build): the
  `hypervisor-test` self test **passed**. The guest printed
  `4f 4b 11 0a` ("OK", I/O APIC version 0x11, newline) and reset. That
  confirms on WHP: partition setup, guest memory mapping, long-mode entry
  (GDT, page tables, segment and control registers), port I/O exits through
  Microsoft's instruction emulator, MMIO exits to bvmm's I/O APIC, and
  reset handling.
- Not yet confirmed: a Linux kernel boot (timers, Hyper-V enlightenments,
  interrupts). The next test is a container with
  `--engine native`.

## First run on Windows: what is most likely to need a fix

These are the assumptions that could not be checked without WHP:

1. **Halt exits.** With WHP's local APIC, `HLT` may never exit (expected) or
   may exit repeatedly while waiting for interrupts. bvmm sleeps 1 ms and
   re-enters; if the guest is slow when idle, this is where to look.
2. **Timer frequencies.** The kernel needs WHP to accept the Hyper-V
   enlightenment bits (`0xbcf`). If that partition property is refused,
   bvmm prints a warning. Without it the kernel tries to calibrate on a PIT
   that bvmm does not emulate.
3. **No 8259 PIC.** bvmm answers the PIC probe so that Linux picks its
   "null" PIC and routes COM1 through the I/O APIC from the MP table.

## Next milestones

1. ~~`hypervisor-test` on the reference laptop (WHP)~~ passed. Next: a
   container boot there, then KVM on Linux (including WSL2 with nested
   virtualization).
2. Network: virtio-net over virtio-mmio, with user-mode NAT.
3. Several vCPUs (APs started with INIT/SIPI through the local APIC).
4. Disks: virtio-blk.
5. Firmware boot (for ISOs and disk images), which is much larger:
   SeaBIOS/OVMF, PCI, ATA/ATAPI, VGA, PS/2.

## Rule for this code

No QEMU source may be copied into bvmm: QEMU is GPL-2.0-only, which cannot
be combined with bContain's GPL-3.0-or-later OR LGPL-3.0-or-later. QEMU's
code was read to learn how WHP behaves; none of it is reused. rust-vmm
crates (Apache-2.0 / BSD-3-Clause) are fine, and `kvm-ioctls` is one.
