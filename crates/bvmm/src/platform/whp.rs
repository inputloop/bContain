// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Windows Hypervisor Platform backend.
//!
//! WinHvPlatform.dll and WinHvEmulation.dll are loaded at run time, so
//! bContain still starts on PCs where the "Windows Hypervisor Platform"
//! feature is off. The FFI below mirrors `winhvplatformdefs.h` and
//! `winhvemulation.h`. Every struct's size and every field offset is
//! checked at compile time against the values in those headers, and
//! `scripts/check-whp-layout.sh` checks the same numbers against the real
//! headers with a C compiler.
//!
//! WHP emulates the local APIC (xAPIC mode). bvmm supplies the I/O APIC,
//! routes its interrupts with `WHvRequestInterrupt`, and uses Microsoft's
//! instruction emulator for port I/O and MMIO exits. The guest kernel sees a
//! Hyper-V partition: it reads the TSC and local APIC timer frequencies from
//! the synthetic MSRs, so no PIT is needed.

#![allow(clippy::upper_case_acronyms)]
// `sym!` transmutes each symbol to the type of the `Api` field it fills,
// which spells out the SDK signature; repeating it at every call adds nothing.
#![allow(clippy::missing_transmute_annotations)]

use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::ptr::null_mut;
use std::sync::Arc;

use crate::boot::{BootState, Segment};
use crate::devices::ioapic::Interrupt;
use crate::devices::{Devices, IrqRoute};
use crate::memory::GuestMemory;
use crate::{Error, Outcome, Result};

type HRESULT = i32;
type Partition = *mut c_void;
type Emulator = *mut c_void;

// ---------------------------------------------------------------------------
// FFI types (winhvplatformdefs.h)

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SegmentRegister {
    base: u64,
    limit: u32,
    selector: u16,
    attributes: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TableRegister {
    pad: [u16; 3],
    limit: u16,
    base: u64,
}

/// WHV_REGISTER_VALUE: a 16-byte, 16-aligned union.
#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct RegisterValue([u64; 2]);

impl RegisterValue {
    fn u64(v: u64) -> Self {
        RegisterValue([v, 0])
    }
    fn segment(s: &Segment) -> Self {
        let r = SegmentRegister {
            base: s.base,
            limit: s.byte_limit(),
            selector: s.selector,
            attributes: s.attributes(),
        };
        // SAFETY: both are 16-byte plain-data types.
        unsafe { std::mem::transmute::<SegmentRegister, RegisterValue>(r) }
    }
    fn table(base: u64, limit: u16) -> Self {
        let t = TableRegister {
            pad: [0; 3],
            limit,
            base,
        };
        // SAFETY: both are 16-byte plain-data types.
        unsafe { std::mem::transmute::<TableRegister, RegisterValue>(t) }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VpExitContext {
    execution_state: u16,
    instruction_length_cr8: u8,
    reserved: u8,
    reserved2: u32,
    cs: SegmentRegister,
    rip: u64,
    rflags: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MemoryAccessContext {
    instruction_byte_count: u8,
    reserved: [u8; 3],
    instruction_bytes: [u8; 16],
    access_info: u32,
    gpa: u64,
    gva: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IoPortAccessContext {
    instruction_byte_count: u8,
    reserved: [u8; 3],
    instruction_bytes: [u8; 16],
    access_info: u32,
    port: u16,
    reserved2: [u16; 3],
    rax: u64,
    rcx: u64,
    rsi: u64,
    rdi: u64,
    ds: SegmentRegister,
    es: SegmentRegister,
}

/// WHV_RUN_VP_EXIT_CONTEXT: the exit-specific part is a union at offset 48.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct RunVpExitContext {
    exit_reason: u32,
    reserved: u32,
    vp: VpExitContext,
    detail: [u64; 22],
}

impl RunVpExitContext {
    fn memory_access(&self) -> &MemoryAccessContext {
        // SAFETY: the union starts at `detail`; this is its MemoryAccess arm.
        unsafe { &*(self.detail.as_ptr() as *const MemoryAccessContext) }
    }
    fn io_port_access(&self) -> &IoPortAccessContext {
        // SAFETY: as above, the IoPortAccess arm.
        unsafe { &*(self.detail.as_ptr() as *const IoPortAccessContext) }
    }
    fn apic_eoi_vector(&self) -> u32 {
        self.detail[0] as u32
    }
    /// WHV_X64_CPUID_ACCESS_CONTEXT: the asked Rax, Rcx, Rdx, Rbx, then the
    /// hypervisor's own answer in the same order.
    fn cpuid(&self) -> (u32, [u64; 4]) {
        let d = &self.detail;
        (d[0] as u32, [d[4], d[5], d[6], d[7]])
    }
}

#[repr(C)]
struct InterruptControl {
    /// Type:8, DestinationMode:4, TriggerMode:4, Reserved:48.
    control: u64,
    destination: u32,
    vector: u32,
}

#[repr(C)]
struct SyntheticFeatureBanks {
    banks_count: u32,
    reserved: u32,
    bank0: u64,
}

#[repr(C)]
struct TranslateGvaResult {
    result_code: u32,
    reserved: u32,
}

// winhvemulation.h

#[repr(C)]
struct EmulatorIoAccess {
    direction: u8,
    port: u16,
    access_size: u16,
    data: u32,
}

#[repr(C)]
struct EmulatorMemoryAccess {
    gpa: u64,
    direction: u8,
    access_size: u8,
    data: [u8; 8],
}

type IoCallback = unsafe extern "system" fn(*mut c_void, *mut EmulatorIoAccess) -> HRESULT;
type MemoryCallback = unsafe extern "system" fn(*mut c_void, *mut EmulatorMemoryAccess) -> HRESULT;
type GetRegistersCallback =
    unsafe extern "system" fn(*mut c_void, *const u32, u32, *mut RegisterValue) -> HRESULT;
type SetRegistersCallback =
    unsafe extern "system" fn(*mut c_void, *const u32, u32, *const RegisterValue) -> HRESULT;
type TranslateCallback =
    unsafe extern "system" fn(*mut c_void, u64, u32, *mut u32, *mut u64) -> HRESULT;

#[repr(C)]
struct EmulatorCallbacks {
    size: u32,
    reserved: u32,
    io: IoCallback,
    memory: MemoryCallback,
    get_registers: GetRegistersCallback,
    set_registers: SetRegistersCallback,
    translate_gva_page: TranslateCallback,
}

// The numbers in these assertions are the C_ASSERTs in winhvplatformdefs.h
// (and, for the emulator types, what scripts/check-whp-layout.sh measures).
const _: () = {
    assert!(size_of::<SegmentRegister>() == 16);
    assert!(size_of::<TableRegister>() == 16);
    assert!(offset_of!(TableRegister, limit) == 6);
    assert!(size_of::<RegisterValue>() == 16);
    assert!(size_of::<VpExitContext>() == 40);
    assert!(offset_of!(VpExitContext, cs) == 8);
    assert!(offset_of!(VpExitContext, rip) == 24);
    assert!(offset_of!(VpExitContext, rflags) == 32);
    assert!(size_of::<MemoryAccessContext>() == 40);
    assert!(offset_of!(MemoryAccessContext, access_info) == 20);
    assert!(offset_of!(MemoryAccessContext, gpa) == 24);
    assert!(size_of::<IoPortAccessContext>() == 96);
    assert!(offset_of!(IoPortAccessContext, port) == 24);
    assert!(offset_of!(IoPortAccessContext, rax) == 32);
    assert!(offset_of!(IoPortAccessContext, ds) == 64);
    assert!(size_of::<RunVpExitContext>() == 224);
    assert!(offset_of!(RunVpExitContext, vp) == 8);
    assert!(offset_of!(RunVpExitContext, detail) == 48);
    assert!(size_of::<InterruptControl>() == 16);
    assert!(size_of::<SyntheticFeatureBanks>() == 16);
    assert!(size_of::<TranslateGvaResult>() == 8);
    assert!(size_of::<EmulatorIoAccess>() == 12);
    assert!(offset_of!(EmulatorIoAccess, port) == 2);
    assert!(offset_of!(EmulatorIoAccess, access_size) == 4);
    assert!(offset_of!(EmulatorIoAccess, data) == 8);
    assert!(size_of::<EmulatorMemoryAccess>() == 24);
    assert!(offset_of!(EmulatorMemoryAccess, direction) == 8);
    assert!(offset_of!(EmulatorMemoryAccess, access_size) == 9);
    assert!(offset_of!(EmulatorMemoryAccess, data) == 10);
    assert!(size_of::<EmulatorCallbacks>() == 48);
};

// Register names (WHV_REGISTER_NAME).
const RAX: u32 = 0x00;
const RCX: u32 = 0x01;
const RDX: u32 = 0x02;
const RBX: u32 = 0x03;
const RSP: u32 = 0x04;
const RSI: u32 = 0x06;
const RIP: u32 = 0x10;
const RFLAGS: u32 = 0x11;
const ES: u32 = 0x12;
const CS: u32 = 0x13;
const SS: u32 = 0x14;
const DS: u32 = 0x15;
const FS: u32 = 0x16;
const GS: u32 = 0x17;
const LDTR: u32 = 0x18;
const TR: u32 = 0x19;
const IDTR: u32 = 0x1a;
const GDTR: u32 = 0x1b;
const CR0: u32 = 0x1c;
const CR3: u32 = 0x1e;
const CR4: u32 = 0x1f;
const EFER: u32 = 0x2001;
/// WHvRegisterInternalActivityState; bit 0 is StartupSuspend ("waiting for
/// the start-up message from the boot processor").
const INTERNAL_ACTIVITY_STATE: u32 = 0x4;

// Partition properties.
const PROP_EXTENDED_VM_EXITS: u32 = 0x1;
const PROP_CPUID_EXIT_LIST: u32 = 0x1003;
const PROP_LOCAL_APIC_EMULATION_MODE: u32 = 0x1005;
const PROP_SYNTHETIC_PROCESSOR_FEATURES_BANKS: u32 = 0x100c;
const PROP_PROCESSOR_COUNT: u32 = 0x1fff;
const LOCAL_APIC_EMULATION_XAPIC: u32 = 1;
/// HypervisorPresent, Hv1, AccessVpRunTimeReg, AccessPartitionReferenceCounter,
/// AccessIntrCtrlRegs, AccessHypercallRegs, AccessVpIndex,
/// AccessPartitionReferenceTsc, AccessFrequencyRegs. Measured from the C
/// bitfield layout by scripts/check-whp-layout.sh.
const SYNTHETIC_FEATURES: u64 = 0xbcf;

// Exit reasons.
const EXIT_MEMORY_ACCESS: u32 = 0x1;
const EXIT_IO_PORT_ACCESS: u32 = 0x2;
const EXIT_UNRECOVERABLE_EXCEPTION: u32 = 0x4;
const EXIT_INVALID_VP_REGISTER_VALUE: u32 = 0x5;
const EXIT_UNSUPPORTED_FEATURE: u32 = 0x6;
const EXIT_INTERRUPT_WINDOW: u32 = 0x7;
const EXIT_HALT: u32 = 0x8;
const EXIT_APIC_EOI: u32 = 0x9;
const EXIT_CPUID: u32 = 0x1001;
const EXIT_CANCELED: u32 = 0x2001;

/// The Hyper-V CPUID leaf that lists what the guest may use, and the bit in
/// its EAX that says the guest may rely on the time-stamp counter
/// (HV_ACCESS_TSC_INVARIANT). WHP has no setting for that bit, and Linux
/// refuses the counter as a clock without it ("Marking TSC unstable due to
/// running on Hyper-V"), leaving a guest that lacks the Hyper-V clock driver
/// (Android's kernel) with a clock that ticks only 250 times a second. WHP
/// keeps the counter steady, so bvmm answers this one leaf itself and adds
/// the bit.
const HV_CPUID_FEATURES: u32 = 0x4000_0003;
const HV_ACCESS_TSC_INVARIANT: u64 = 1 << 15;

const MAP_READ_WRITE_EXECUTE: u32 = 0x7;
const S_OK: HRESULT = 0;
const E_FAIL: HRESULT = 0x8000_4005u32 as i32;

// ---------------------------------------------------------------------------
// Loading the DLLs

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}

struct Api {
    create_partition: unsafe extern "system" fn(*mut Partition) -> HRESULT,
    setup_partition: unsafe extern "system" fn(Partition) -> HRESULT,
    delete_partition: unsafe extern "system" fn(Partition) -> HRESULT,
    set_partition_property:
        unsafe extern "system" fn(Partition, u32, *const c_void, u32) -> HRESULT,
    map_gpa_range: unsafe extern "system" fn(Partition, *mut c_void, u64, u64, u32) -> HRESULT,
    unmap_gpa_range: unsafe extern "system" fn(Partition, u64, u64) -> HRESULT,
    create_virtual_processor: unsafe extern "system" fn(Partition, u32, u32) -> HRESULT,
    delete_virtual_processor: unsafe extern "system" fn(Partition, u32) -> HRESULT,
    run_virtual_processor: unsafe extern "system" fn(Partition, u32, *mut c_void, u32) -> HRESULT,
    cancel_run: unsafe extern "system" fn(Partition, u32, u32) -> HRESULT,
    get_registers:
        unsafe extern "system" fn(Partition, u32, *const u32, u32, *mut RegisterValue) -> HRESULT,
    set_registers:
        unsafe extern "system" fn(Partition, u32, *const u32, u32, *const RegisterValue) -> HRESULT,
    request_interrupt:
        unsafe extern "system" fn(Partition, *const InterruptControl, u32) -> HRESULT,
    translate_gva: unsafe extern "system" fn(
        Partition,
        u32,
        u64,
        u32,
        *mut TranslateGvaResult,
        *mut u64,
    ) -> HRESULT,
    emulator_create: unsafe extern "system" fn(*const EmulatorCallbacks, *mut Emulator) -> HRESULT,
    emulator_destroy: unsafe extern "system" fn(Emulator) -> HRESULT,
    emulator_try_io: unsafe extern "system" fn(
        Emulator,
        *mut c_void,
        *const VpExitContext,
        *const IoPortAccessContext,
        *mut u32,
    ) -> HRESULT,
    emulator_try_mmio: unsafe extern "system" fn(
        Emulator,
        *mut c_void,
        *const VpExitContext,
        *const MemoryAccessContext,
        *mut u32,
    ) -> HRESULT,
}

fn load_library(name: &str) -> Result<*mut c_void> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 string.
    let h = unsafe { LoadLibraryW(wide.as_ptr()) };
    if h.is_null() {
        return Err(Error::new(format!(
            "{name} could not be loaded: turn on \"Windows Hypervisor Platform\" \
             (Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform) and reboot"
        )));
    }
    Ok(h)
}

macro_rules! sym {
    ($module:expr, $name:literal) => {{
        // SAFETY: the name is NUL-terminated; the symbol's signature is the
        // one declared in the field this initializes (from the SDK headers).
        let p = unsafe { GetProcAddress($module, concat!($name, "\0").as_ptr()) };
        if p.is_null() {
            return Err(Error::new(concat!(
                $name,
                " is missing: this Windows build's hypervisor platform is too old (update Windows)"
            )));
        }
        unsafe { std::mem::transmute::<*mut c_void, _>(p) }
    }};
}

impl Api {
    fn load() -> Result<Api> {
        let hv = load_library("WinHvPlatform.dll")?;
        let emu = load_library("WinHvEmulation.dll")?;
        Ok(Api {
            create_partition: sym!(hv, "WHvCreatePartition"),
            setup_partition: sym!(hv, "WHvSetupPartition"),
            delete_partition: sym!(hv, "WHvDeletePartition"),
            set_partition_property: sym!(hv, "WHvSetPartitionProperty"),
            map_gpa_range: sym!(hv, "WHvMapGpaRange"),
            unmap_gpa_range: sym!(hv, "WHvUnmapGpaRange"),
            create_virtual_processor: sym!(hv, "WHvCreateVirtualProcessor"),
            delete_virtual_processor: sym!(hv, "WHvDeleteVirtualProcessor"),
            run_virtual_processor: sym!(hv, "WHvRunVirtualProcessor"),
            cancel_run: sym!(hv, "WHvCancelRunVirtualProcessor"),
            get_registers: sym!(hv, "WHvGetVirtualProcessorRegisters"),
            set_registers: sym!(hv, "WHvSetVirtualProcessorRegisters"),
            request_interrupt: sym!(hv, "WHvRequestInterrupt"),
            translate_gva: sym!(hv, "WHvTranslateGva"),
            emulator_create: sym!(emu, "WHvEmulatorCreateEmulator"),
            emulator_destroy: sym!(emu, "WHvEmulatorDestroyEmulator"),
            emulator_try_io: sym!(emu, "WHvEmulatorTryIoEmulation"),
            emulator_try_mmio: sym!(emu, "WHvEmulatorTryMmioEmulation"),
        })
    }
}

fn check(hr: HRESULT, what: &str) -> Result<()> {
    if hr < 0 {
        Err(Error::new(format!(
            "WHP: {what} failed (HRESULT {:#010x})",
            hr as u32
        )))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Running

/// Deletes the partition and its vCPUs however `run` exits.
struct Vm<'a> {
    api: &'a Api,
    partition: Partition,
    vcpus: u32,
}

impl Drop for Vm<'_> {
    fn drop(&mut self) {
        // SAFETY: handles created by this module and not yet released.
        unsafe {
            for vp in 0..self.vcpus {
                (self.api.delete_virtual_processor)(self.partition, vp);
            }
            (self.api.delete_partition)(self.partition);
        }
    }
}

/// What the emulator callbacks need. Passed as their `Context` pointer.
struct CallbackContext<'a> {
    api: &'a Api,
    partition: Partition,
    vp: u32,
    mem: &'a GuestMemory,
    devices: &'a Devices,
}

unsafe extern "system" fn io_callback(ctx: *mut c_void, access: *mut EmulatorIoAccess) -> HRESULT {
    let c = &*(ctx as *const CallbackContext);
    let a = &mut *access;
    let size = a.access_size as u8;
    if a.direction == 0 {
        a.data = c.devices.io_in(a.port, size);
    } else {
        c.devices.io_out(a.port, size, a.data);
    }
    S_OK
}

unsafe extern "system" fn memory_callback(
    ctx: *mut c_void,
    access: *mut EmulatorMemoryAccess,
) -> HRESULT {
    let c = &*(ctx as *const CallbackContext);
    let a = &mut *access;
    let n = (a.access_size as usize).min(8);
    let in_ram = c.mem.contains(a.gpa, n);
    if a.direction == 0 {
        if in_ram {
            if c.mem.read(a.gpa, &mut a.data[..n]).is_err() {
                return E_FAIL;
            }
        } else {
            let v = c.devices.mmio_read(a.gpa, n as u8).to_le_bytes();
            a.data[..n].copy_from_slice(&v[..n]);
        }
    } else if in_ram {
        if c.mem.write(a.gpa, &a.data[..n]).is_err() {
            return E_FAIL;
        }
    } else {
        let mut b = [0u8; 8];
        b[..n].copy_from_slice(&a.data[..n]);
        c.devices.mmio_write(a.gpa, n as u8, u64::from_le_bytes(b));
    }
    S_OK
}

unsafe extern "system" fn get_registers_callback(
    ctx: *mut c_void,
    names: *const u32,
    count: u32,
    values: *mut RegisterValue,
) -> HRESULT {
    let c = &*(ctx as *const CallbackContext);
    (c.api.get_registers)(c.partition, c.vp, names, count, values)
}

unsafe extern "system" fn set_registers_callback(
    ctx: *mut c_void,
    names: *const u32,
    count: u32,
    values: *const RegisterValue,
) -> HRESULT {
    let c = &*(ctx as *const CallbackContext);
    (c.api.set_registers)(c.partition, c.vp, names, count, values)
}

unsafe extern "system" fn translate_callback(
    ctx: *mut c_void,
    gva: u64,
    flags: u32,
    result: *mut u32,
    gpa: *mut u64,
) -> HRESULT {
    let c = &*(ctx as *const CallbackContext);
    let mut r = TranslateGvaResult {
        result_code: 0,
        reserved: 0,
    };
    let hr = (c.api.translate_gva)(c.partition, c.vp, gva, flags, &mut r, gpa);
    *result = r.result_code;
    hr
}

/// A partition handle that may be used from the console-input thread.
#[derive(Clone, Copy)]
struct SendPartition(Partition);
// SAFETY: WHP partition handles are usable from any thread; WHvRequestInterrupt is thread-safe.
unsafe impl Send for SendPartition {}
unsafe impl Sync for SendPartition {}

/// Maps host memory into the guest with `WHvMapGpaRange`.
struct WhpMapper {
    api: &'static Api,
    partition: SendPartition,
}

impl crate::memory::Mapper for WhpMapper {
    unsafe fn map(&self, gpa: u64, host: *mut u8, len: u64) -> Result<()> {
        /// Read and write; device memory is not for running code.
        const MAP_READ_WRITE: u32 = 0x3;
        // SAFETY: the caller keeps the host range valid until `unmap`.
        let hr = unsafe {
            (self.api.map_gpa_range)(
                self.partition.0,
                host as *mut c_void,
                gpa,
                len,
                MAP_READ_WRITE,
            )
        };
        check(hr, "mapping host memory into the guest")
    }

    fn unmap(&self, gpa: u64, len: u64) -> Result<()> {
        // SAFETY: removes a mapping; nothing is dereferenced.
        let hr = unsafe { (self.api.unmap_gpa_range)(self.partition.0, gpa, len) };
        check(hr, "unmapping host memory from the guest")
    }
}

fn set_property<T>(api: &Api, p: Partition, code: u32, value: &T, what: &str) -> Result<()> {
    // SAFETY: `value` points to a live T of the stated size.
    let hr = unsafe {
        (api.set_partition_property)(
            p,
            code,
            value as *const T as *const c_void,
            size_of::<T>() as u32,
        )
    };
    check(hr, what)
}

pub fn run(
    mem: &GuestMemory,
    boot: &BootState,
    devices: Arc<Devices>,
    cpus: u8,
) -> Result<Outcome> {
    let count = cpus.max(1) as u32;
    // Leaked on purpose: the interrupt route outlives this call by a moment
    // when the input thread races shutdown, and the DLLs stay loaded anyway.
    let api: &'static Api = Box::leak(Box::new(Api::load()?));

    let mut partition: Partition = null_mut();
    // SAFETY: out-pointer to a local.
    check(
        unsafe { (api.create_partition)(&mut partition) },
        "creating the partition",
    )?;
    let mut vm = Vm {
        api,
        partition,
        vcpus: 0,
    };

    set_property(
        api,
        partition,
        PROP_PROCESSOR_COUNT,
        &count,
        "setting the processor count",
    )?;
    set_property(
        api,
        partition,
        PROP_LOCAL_APIC_EMULATION_MODE,
        &LOCAL_APIC_EMULATION_XAPIC,
        "enabling local APIC emulation",
    )
    .map_err(|e| {
        Error::new(format!(
            "{e}: this Windows build cannot emulate a local APIC (Windows 10 2004+ needed)"
        ))
    })?;
    let synthetic = SyntheticFeatureBanks {
        banks_count: 1,
        reserved: 0,
        bank0: SYNTHETIC_FEATURES,
    };
    if let Err(e) = set_property(
        api,
        partition,
        PROP_SYNTHETIC_PROCESSOR_FEATURES_BANKS,
        &synthetic,
        "enabling Hyper-V enlightenments",
    ) {
        // Without them the kernel must calibrate its timers on a PIT bvmm does not emulate.
        eprintln!("bvmm: warning: {e}; the guest may not find a timer");
    }
    // Ask to answer one CPUID leaf ourselves; see HV_CPUID_FEATURES. If
    // this Windows cannot do that, the guest only loses its precise clock.
    let cpuid_exits = set_property(
        api,
        partition,
        PROP_EXTENDED_VM_EXITS,
        &1u64,
        "enabling CPUID exits",
    )
    .and_then(|()| {
        set_property(
            api,
            partition,
            PROP_CPUID_EXIT_LIST,
            &[HV_CPUID_FEATURES],
            "choosing the CPUID leaf to answer",
        )
    });
    if let Err(e) = cpuid_exits {
        eprintln!("bvmm: warning: {e}; the guest's clock may be coarse");
    }
    // SAFETY: valid partition handle.
    check(
        unsafe { (api.setup_partition)(partition) },
        "setting up the partition",
    )?;
    for (gpa, host, len) in mem.regions() {
        // SAFETY: the range is part of `mem`'s live allocation, which outlives the partition.
        check(
            unsafe {
                (api.map_gpa_range)(
                    partition,
                    host as *mut c_void,
                    gpa,
                    len,
                    MAP_READ_WRITE_EXECUTE,
                )
            },
            "mapping guest memory",
        )?;
    }
    for vp in 0..count {
        // SAFETY: valid partition handle.
        check(
            unsafe { (api.create_virtual_processor)(partition, vp, 0) },
            "creating a virtual processor",
        )?;
        vm.vcpus = vp + 1;
        if vp > 0 {
            // Secondary processors wait for the boot processor to start
            // them; the hypervisor's local APIC then does the starting.
            let suspended = RegisterValue::u64(1);
            // SAFETY: one name, one value; valid partition and vCPU.
            check(
                unsafe {
                    (api.set_registers)(partition, vp, &INTERNAL_ACTIVITY_STATE, 1, &suspended)
                },
                "parking a secondary processor",
            )?;
        }
    }

    let names = [
        RIP, RSI, RSP, RFLAGS, CS, DS, ES, FS, GS, SS, TR, LDTR, GDTR, IDTR, CR0, CR3, CR4, EFER,
    ];
    let ds = RegisterValue::segment(&boot.ds);
    let values = [
        RegisterValue::u64(boot.rip),
        RegisterValue::u64(boot.rsi),
        RegisterValue::u64(boot.rsp),
        RegisterValue::u64(boot.rflags),
        RegisterValue::segment(&boot.cs),
        ds,
        ds,
        ds,
        ds,
        ds,
        RegisterValue::segment(&boot.tr),
        RegisterValue::segment(&boot.ldt),
        RegisterValue::table(boot.gdt_base, boot.gdt_limit),
        RegisterValue::table(0, 0),
        RegisterValue::u64(boot.cr0),
        RegisterValue::u64(boot.cr3),
        RegisterValue::u64(boot.cr4),
        RegisterValue::u64(boot.efer),
    ];
    // SAFETY: arrays of equal length; valid partition and vCPU.
    check(
        unsafe {
            (api.set_registers)(
                partition,
                0,
                names.as_ptr(),
                names.len() as u32,
                values.as_ptr(),
            )
        },
        "setting the boot registers",
    )?;

    let p = SendPartition(partition);
    let deliver = move |i: Interrupt| {
        let p = p;
        let control = InterruptControl {
            control: (i.delivery_mode as u64 & 0xff)
                | (i.logical as u64) << 8
                | (i.level as u64) << 12,
            destination: i.destination as u32,
            vector: i.vector as u32,
        };
        // SAFETY: valid partition; struct size passed alongside.
        let _ =
            unsafe { (api.request_interrupt)(p.0, &control, size_of::<InterruptControl>() as u32) };
    };
    devices.set_mapper(Box::new(WhpMapper { api, partition: p }));
    devices.set_irq_route(IrqRoute::Ioapic(Box::new(deliver)));
    // An MSI message names its local APIC interrupt directly.
    devices.set_msi_route(Box::new(move |address, data| {
        deliver(crate::devices::pci::decode_msi(address, data))
    }));

    let stop = std::sync::atomic::AtomicBool::new(false);
    let (done_tx, done_rx) = std::sync::mpsc::channel::<Result<Outcome>>();
    let devices = &*devices;
    let stop = &stop;
    std::thread::scope(|scope| {
        for vp in 0..count {
            let done_tx = done_tx.clone();
            scope.spawn(move || {
                let p = p;
                let _ = done_tx.send(run_vcpu(api, p.0, vp, mem, devices, stop));
            });
        }
        drop(done_tx);
        // The first processor to finish decides the outcome.
        let outcome = done_rx
            .recv()
            .unwrap_or_else(|_| Err(Error::new("WHP: the vCPU threads vanished")));
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // Bring the others out of the hypervisor. A cancel can arrive just
        // before a processor goes back in, so repeat until all have left.
        let mut left = count - 1;
        while left > 0 {
            for vp in 0..count {
                // SAFETY: valid partition; cancelling an idle vCPU is harmless.
                unsafe { (api.cancel_run)(p.0, vp, 0) };
            }
            if done_rx
                .recv_timeout(std::time::Duration::from_millis(10))
                .is_ok()
            {
                left -= 1;
            }
        }
        outcome
    })
}

/// Run one virtual processor until the guest resets, halts or fails, or
/// `stop` is set.
fn run_vcpu(
    api: &'static Api,
    partition: Partition,
    vp: u32,
    mem: &GuestMemory,
    devices: &Devices,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<Outcome> {
    /// Destroys the instruction emulator when the processor stops.
    struct OwnedEmulator(&'static Api, Emulator);
    impl Drop for OwnedEmulator {
        fn drop(&mut self) {
            // SAFETY: created by emulator_create below, destroyed once.
            unsafe { (self.0.emulator_destroy)(self.1) };
        }
    }
    let callbacks = EmulatorCallbacks {
        size: size_of::<EmulatorCallbacks>() as u32,
        reserved: 0,
        io: io_callback,
        memory: memory_callback,
        get_registers: get_registers_callback,
        set_registers: set_registers_callback,
        translate_gva_page: translate_callback,
    };
    let mut emulator: Emulator = null_mut();
    // SAFETY: callbacks struct is valid for the call; out-pointer to a local.
    check(
        unsafe { (api.emulator_create)(&callbacks, &mut emulator) },
        "creating the instruction emulator",
    )?;
    let emulator = OwnedEmulator(api, emulator);

    let mut cb = CallbackContext {
        api,
        partition,
        vp,
        mem,
        devices,
    };
    let cb_ptr = &mut cb as *mut CallbackContext as *mut c_void;
    let mut ctx: RunVpExitContext = RunVpExitContext {
        exit_reason: 0,
        reserved: 0,
        vp: VpExitContext {
            execution_state: 0,
            instruction_length_cr8: 0,
            reserved: 0,
            reserved2: 0,
            cs: SegmentRegister::default(),
            rip: 0,
            rflags: 0,
        },
        detail: [0; 22],
    };
    loop {
        if devices.reset_requested() || stop.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(Outcome::Reset);
        }
        // SAFETY: ctx is a correctly sized, aligned exit-context buffer.
        let hr = unsafe {
            (api.run_virtual_processor)(
                partition,
                vp,
                &mut ctx as *mut _ as *mut c_void,
                size_of::<RunVpExitContext>() as u32,
            )
        };
        check(hr, "running the virtual processor")?;
        match ctx.exit_reason {
            EXIT_IO_PORT_ACCESS => {
                let mut status = 0u32;
                // SAFETY: contexts come from this exit; cb outlives the call.
                let hr = unsafe {
                    (api.emulator_try_io)(
                        emulator.1,
                        cb_ptr,
                        &ctx.vp,
                        ctx.io_port_access(),
                        &mut status,
                    )
                };
                if hr < 0 || status & 1 == 0 {
                    return Err(Error::new(format!(
                        "could not emulate port I/O on port {:#x} at rip {:#x} (status {status:#x})",
                        ctx.io_port_access().port,
                        ctx.vp.rip
                    )));
                }
            }
            EXIT_MEMORY_ACCESS => {
                let mut status = 0u32;
                // SAFETY: as above.
                let hr = unsafe {
                    (api.emulator_try_mmio)(
                        emulator.1,
                        cb_ptr,
                        &ctx.vp,
                        ctx.memory_access(),
                        &mut status,
                    )
                };
                if hr < 0 || status & 1 == 0 {
                    return Err(Error::new(format!(
                        "could not emulate a memory access to {:#x} at rip {:#x} (status {status:#x})",
                        ctx.memory_access().gpa,
                        ctx.vp.rip
                    )));
                }
            }
            EXIT_HALT => {
                // Only the boot processor halting for good ends the guest;
                // Linux parks the others that way when it shuts down.
                if ctx.vp.rflags & 0x200 == 0 && vp == 0 {
                    return Ok(Outcome::Halted);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            EXIT_APIC_EOI => devices.eoi(ctx.apic_eoi_vector() as u8),
            EXIT_CPUID => {
                let (leaf, mut answer) = ctx.cpuid();
                if leaf == HV_CPUID_FEATURES {
                    answer[0] |= HV_ACCESS_TSC_INVARIANT;
                }
                let names = [RAX, RCX, RDX, RBX, RIP];
                let next = ctx.vp.rip + (ctx.vp.instruction_length_cr8 & 0xf) as u64;
                let values = [
                    RegisterValue::u64(answer[0]),
                    RegisterValue::u64(answer[1]),
                    RegisterValue::u64(answer[2]),
                    RegisterValue::u64(answer[3]),
                    RegisterValue::u64(next),
                ];
                // SAFETY: arrays of equal length; valid partition and vCPU.
                check(
                    unsafe {
                        (api.set_registers)(
                            partition,
                            vp,
                            names.as_ptr(),
                            names.len() as u32,
                            values.as_ptr(),
                        )
                    },
                    "answering CPUID",
                )?;
            }
            EXIT_INTERRUPT_WINDOW | EXIT_CANCELED => {}
            EXIT_UNRECOVERABLE_EXCEPTION => {
                return Err(Error::new(format!(
                    "the guest hit an unrecoverable exception (triple fault) at rip {:#x}",
                    ctx.vp.rip
                )))
            }
            EXIT_INVALID_VP_REGISTER_VALUE => {
                return Err(Error::new(
                    "WHP rejected the virtual processor's register state (a bvmm bug)",
                ))
            }
            EXIT_UNSUPPORTED_FEATURE => {
                return Err(Error::new(format!(
                    "the guest used a feature WHP does not support (rip {:#x})",
                    ctx.vp.rip
                )))
            }
            other => {
                return Err(Error::new(format!(
                    "unexpected WHP exit {other:#x} at rip {:#x}",
                    ctx.vp.rip
                )))
            }
        }
    }
}
