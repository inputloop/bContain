<!-- SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors -->
<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# Native VMM roadmap (`crates/bvmm`)

bContain ships on QEMU today. The plan is for bContain to grow its own
hypervisor without the rest of the code noticing. This page describes the
seams that already exist and the order the native VMM gets built in.

## Seams that exist today

| Seam | Where | What it decouples |
|---|---|---|
| `bvmm::probe()` / `Accel` | `crates/bvmm/src/lib.rs` | Detection of hardware acceleration (KVM on Linux, WHP on Windows). The QEMU backend already uses it to choose `-accel`. |
| `VmSpec` | `crates/bcontain/src/engine.rs` | A backend-neutral machine description: vCPUs, RAM, disks, direct kernel boot, display, ports. |
| `Hypervisor` trait | `crates/bcontain/src/engine.rs` | `launch` / `run_foreground` / `stop` / `is_alive`. The CLI, the web UI and the OCI runner only call these. |
| `engine::backend("native")` | `crates/bcontain/src/engine.rs` | Already routed. It returns `NotYetImplemented` until `NativeVmm::new()` succeeds. |

## Milestones

1. **Accelerator bindings.** On Linux: KVM ioctls (`KVM_CREATE_VM`,
   `KVM_SET_USER_MEMORY_REGION`, `KVM_CREATE_VCPU`, `KVM_RUN`). On Windows: WHP
   (`WHvCreatePartition`, `WHvMapGpaRange`, `WHvRunVirtualProcessor`). One
   `trait Partition` covers both.
2. **Direct kernel boot.** Load a bzImage and initramfs using the Linux boot
   protocol, build an e820 map, then enter 64-bit long mode. This is exactly
   the path container micro-VMs use, so it is the first workload to move off
   QEMU.
3. **Minimal devices.** A 16550 serial port for the console, virtio-mmio
   transport, virtio-blk, virtio-net (user-mode NAT), and virtio-rng.
4. **Swap the container path.** `oci` + `ops::create` select the native
   backend for containers when `probe().usable`. QEMU stays the fallback.
5. **Full VMs.** PCI, ACPI and firmware boot (OVMF), then virtio-gpu.
   Until this milestone lands, disk-image VMs keep using QEMU.

## Rules for the native backend

- It must pass the same `VmSpec` tests the QEMU backend passes.
- If acceleration is missing, it must refuse cleanly. It never falls back to
  emulating guest CPUs in software.
- It must not need administrator rights on Windows beyond enabling the
  "Windows Hypervisor Platform" feature.
