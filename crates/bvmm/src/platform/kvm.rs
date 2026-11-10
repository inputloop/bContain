// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! KVM backend (Linux), through rust-vmm's kvm-ioctls. KVM supplies the
//! interrupt controllers (PIC, I/O APIC, local APIC) and the PIT in the
//! kernel, so only the slow legacy devices live in [`Devices`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use kvm_bindings::{
    kvm_fpu, kvm_msi, kvm_pit_config, kvm_regs, kvm_segment, kvm_userspace_memory_region,
    KVM_MAX_CPUID_ENTRIES, KVM_PIT_SPEAKER_DUMMY,
};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd};

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

pub fn run(
    mem: &GuestMemory,
    boot: &BootState,
    devices: Arc<Devices>,
    cpus: u8,
) -> Result<Outcome> {
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
    for (slot, (gpa, host, len)) in mem.regions().into_iter().enumerate() {
        let region = kvm_userspace_memory_region {
            slot: slot as u32,
            flags: 0,
            guest_phys_addr: gpa,
            memory_size: len,
            userspace_addr: host as u64,
        };
        // SAFETY: the region is part of `mem`'s live allocation, which
        // outlives the VM (the VM is dropped at the end of this function).
        unsafe { vm.set_user_memory_region(region) }.map_err(err("mapping guest memory"))?;
    }

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

    let cpuid = kvm
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(err("reading CPUID"))?;
    let mut vcpus = Vec::new();
    for id in 0..cpus {
        let mut vcpu = vm.create_vcpu(id as u64).map_err(err("creating a vCPU"))?;
        let mut cpuid = cpuid.clone();
        for e in cpuid.as_mut_slice() {
            match e.function {
                // Initial APIC ID, and how many logical processors there are.
                1 => e.ebx = (e.ebx & 0x0000_ffff) | (cpus as u32) << 16 | (id as u32) << 24,
                // x2APIC ID.
                0xb | 0x1f => e.edx = id as u32,
                _ => {}
            }
        }
        vcpu.set_cpuid2(&cpuid).map_err(err("setting CPUID"))?;
        if id == 0 {
            set_boot_state(&mut vcpu, boot)?;
        }
        // The other processors wait, inside KVM, for the kernel's INIT and
        // startup IPIs: that is what a vCPU with a non-zero ID does when the
        // local APIC is in the kernel.
        vcpus.push(vcpu);
    }

    install_kick_handler();
    let stop = AtomicBool::new(false);
    let (id_tx, id_rx) = mpsc::channel::<libc::pthread_t>();
    let (done_tx, done_rx) = mpsc::channel::<Result<Outcome>>();
    std::thread::scope(|scope| {
        let count = vcpus.len();
        for mut vcpu in vcpus {
            let (id_tx, done_tx) = (id_tx.clone(), done_tx.clone());
            let (devices, stop) = (&devices, &stop);
            scope.spawn(move || {
                // SAFETY: pthread_self has no preconditions.
                let _ = id_tx.send(unsafe { libc::pthread_self() });
                let _ = done_tx.send(run_vcpu(&mut vcpu, devices, stop));
            });
        }
        let threads: Vec<libc::pthread_t> = id_rx.iter().take(count).collect();
        // The first vCPU to finish decides how the run ended.
        let outcome = done_rx
            .recv()
            .unwrap_or_else(|_| Err(Error::new("KVM: the vCPU threads vanished")));
        stop.store(true, Ordering::SeqCst);
        // A vCPU waiting inside KVM only notices `stop` when a signal
        // interrupts it; keep nudging until every thread has reported.
        let mut finished = 1;
        while finished < count {
            for t in &threads {
                // SAFETY: the threads are alive until the scope ends.
                unsafe { libc::pthread_kill(*t, kick_signal()) };
            }
            while done_rx.recv_timeout(Duration::from_millis(2)).is_ok() {
                finished += 1;
            }
        }
        outcome
    })
}

fn kick_signal() -> libc::c_int {
    libc::SIGRTMIN()
}

extern "C" fn kick_handler(_: libc::c_int) {}

/// Make the kick signal interrupt a blocked `KVM_RUN` instead of killing the
/// process or restarting the call.
fn install_kick_handler() {
    // SAFETY: a zeroed sigaction with a valid handler and no flags is well formed.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = kick_handler as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(kick_signal(), &action, std::ptr::null_mut());
    }
}

/// Put the boot processor in the state the loader computed.
fn set_boot_state(vcpu: &mut VcpuFd, boot: &BootState) -> Result<()> {
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
    vcpu.set_regs(&regs).map_err(err("setting registers"))
}

/// Run one vCPU until the guest resets, it fails, or `stop` is set.
fn run_vcpu(vcpu: &mut VcpuFd, devices: &Devices, stop: &AtomicBool) -> Result<Outcome> {
    loop {
        if devices.reset_requested() || stop.load(Ordering::SeqCst) {
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
