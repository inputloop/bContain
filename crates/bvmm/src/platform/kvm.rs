// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! KVM backend (Linux), through rust-vmm's kvm-ioctls. KVM supplies the
//! interrupt controllers (PIC, I/O APIC, local APIC) and the PIT in the
//! kernel, so only the slow legacy devices live in [`Devices`].

use std::sync::Arc;

use kvm_bindings::{
    kvm_fpu, kvm_msi, kvm_pit_config, kvm_regs, kvm_segment, kvm_userspace_memory_region,
    KVM_MAX_CPUID_ENTRIES, KVM_PIT_SPEAKER_DUMMY,
};
use kvm_ioctls::{Kvm, VcpuExit};

use crate::boot::{BootState, Segment};
use crate::devices::{Devices, IrqRoute};
use crate::memory::GuestMemory;
use crate::{Error, Outcome, Result};

const EINTR: i32 = 4;
const EAGAIN: i32 = 11;
/// Intel VMX needs a 3-page TSS region for real-mode emulation; keep it in the MMIO hole.
const TSS_ADDR: usize = 0xfffb_d000;

fn err(what: &str) -> impl Fn(kvm_ioctls::Error) -> Error + '_ {
    move |e| Error::new(format!("KVM: {what}: {e}"))
}

fn segment(s: &Segment, unusable: bool) -> kvm_segment {
    kvm_segment {
        base: s.base,
        limit: s.byte_limit(),
        selector: s.selector,
        type_: s.type_,
        present: s.present as u8,
        dpl: s.dpl,
        db: s.db as u8,
        s: s.s as u8,
        l: s.l as u8,
        g: s.g as u8,
        avl: s.avl as u8,
        unusable: unusable as u8,
        padding: 0,
    }
}

pub fn run(mem: &GuestMemory, boot: &BootState, devices: Arc<Devices>) -> Result<Outcome> {
    let kvm = Kvm::new().map_err(err("opening /dev/kvm"))?;
    let vm = Arc::new(kvm.create_vm().map_err(err("creating the VM"))?);
    vm.set_tss_address(TSS_ADDR)
        .map_err(err("setting the TSS address"))?;
    vm.create_irq_chip()
        .map_err(err("creating the interrupt controllers"))?;
    let pit = kvm_pit_config {
        flags: KVM_PIT_SPEAKER_DUMMY,
        ..Default::default()
    };
    vm.create_pit2(pit).map_err(err("creating the PIT"))?;
    let region = kvm_userspace_memory_region {
        slot: 0,
        flags: 0,
        guest_phys_addr: 0,
        memory_size: mem.size(),
        userspace_addr: mem.host_ptr() as u64,
    };
    // SAFETY: the region is `mem`'s live allocation, which outlives the VM
    // (the VM is dropped at the end of this function).
    unsafe { vm.set_user_memory_region(region) }.map_err(err("mapping guest memory"))?;

    let line_vm = vm.clone();
    devices.set_irq_route(IrqRoute::KernelLine(Box::new(move |irq| {
        // ISA interrupts are edges: raise and lower the line.
        let _ = line_vm.set_irq_line(irq as u32, true);
        let _ = line_vm.set_irq_line(irq as u32, false);
    })));

    let msi_vm = vm.clone();
    devices.set_msi_route(Box::new(move |address, data| {
        let msi = kvm_msi {
            address_lo: address as u32,
            address_hi: (address >> 32) as u32,
            data,
            ..Default::default()
        };
        let _ = msi_vm.signal_msi(msi);
    }));

    let mut vcpu = vm.create_vcpu(0).map_err(err("creating the vCPU"))?;
    let cpuid = kvm
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(err("reading CPUID"))?;
    vcpu.set_cpuid2(&cpuid).map_err(err("setting CPUID"))?;

    let mut sregs = vcpu.get_sregs().map_err(err("reading special registers"))?;
    sregs.cs = segment(&boot.cs, false);
    let ds = segment(&boot.ds, false);
    (sregs.ds, sregs.es, sregs.fs, sregs.gs, sregs.ss) = (ds, ds, ds, ds, ds);
    sregs.tr = segment(&boot.tr, false);
    sregs.ldt = segment(&boot.ldt, true);
    sregs.gdt.base = boot.gdt_base;
    sregs.gdt.limit = boot.gdt_limit;
    sregs.idt.base = 0;
    sregs.idt.limit = 0;
    sregs.cr0 = boot.cr0;
    sregs.cr3 = boot.cr3;
    sregs.cr4 = boot.cr4;
    sregs.efer = boot.efer;
    vcpu.set_sregs(&sregs)
        .map_err(err("setting special registers"))?;
    let fpu = kvm_fpu {
        fcw: 0x37f,
        mxcsr: 0x1f80,
        ..Default::default()
    };
    vcpu.set_fpu(&fpu).map_err(err("setting FPU state"))?;
    let regs = kvm_regs {
        rip: boot.rip,
        rsi: boot.rsi,
        rsp: boot.rsp,
        rflags: boot.rflags,
        ..Default::default()
    };
    vcpu.set_regs(&regs).map_err(err("setting registers"))?;

    loop {
        if devices.reset_requested() {
            return Ok(Outcome::Reset);
        }
        match vcpu.run() {
            Ok(VcpuExit::IoIn(port, data)) => {
                let v = devices.io_in(port, data.len() as u8).to_le_bytes();
                let n = data.len().min(4);
                data[..n].copy_from_slice(&v[..n]);
            }
            Ok(VcpuExit::IoOut(port, data)) => {
                let mut b = [0u8; 4];
                let n = data.len().min(4);
                b[..n].copy_from_slice(&data[..n]);
                devices.io_out(port, n as u8, u32::from_le_bytes(b));
            }
            Ok(VcpuExit::MmioRead(addr, data)) => {
                let v = devices.mmio_read(addr, data.len() as u8).to_le_bytes();
                let n = data.len().min(8);
                data[..n].copy_from_slice(&v[..n]);
            }
            Ok(VcpuExit::MmioWrite(addr, data)) => {
                let mut b = [0u8; 8];
                let n = data.len().min(8);
                b[..n].copy_from_slice(&data[..n]);
                devices.mmio_write(addr, n as u8, u64::from_le_bytes(b));
            }
            Ok(VcpuExit::Hlt) => return Ok(Outcome::Halted),
            Ok(VcpuExit::Shutdown) => {
                return Err(Error::new(
                    "the guest CPU shut down (triple fault): the kernel crashed very early",
                ))
            }
            Ok(VcpuExit::SystemEvent(..)) => return Ok(Outcome::Reset),
            Ok(VcpuExit::FailEntry(reason, _)) => {
                return Err(Error::new(format!(
                    "KVM could not enter the guest (hardware reason {reason:#x})"
                )))
            }
            Ok(VcpuExit::InternalError) => {
                return Err(Error::new("KVM internal error while running the guest"))
            }
            Ok(other) => return Err(Error::new(format!("KVM: unexpected exit {other:?}"))),
            Err(e) if e.errno() == EINTR || e.errno() == EAGAIN => continue,
            Err(e) => return Err(Error::new(format!("KVM: running the vCPU: {e}"))),
        }
    }
}
