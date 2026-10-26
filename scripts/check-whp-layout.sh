#!/bin/sh
# SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
# SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
# Check that the WHP FFI in crates/bvmm/src/platform/whp.rs matches
# Microsoft's headers: every size, offset and constant it hard-codes is
# asserted here against winhvplatform(defs).h / winhvemulation.h with a C
# compiler. Needs the MinGW-w64 cross compiler (apt install gcc-mingw-w64).
set -eu
cc=${CC:-x86_64-w64-mingw32-gcc}
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
cat > "$dir/layout.c" <<'C'
#define _WIN32_WINNT 0x0A00
#include <stddef.h>
#include <windows.h>
#include <winhvplatform.h>
#include <winhvemulation.h>
#define CHECK(x) _Static_assert(x, #x)
CHECK(sizeof(WHV_X64_SEGMENT_REGISTER) == 16);
CHECK(offsetof(WHV_X64_TABLE_REGISTER, Limit) == 6);
CHECK(sizeof(WHV_REGISTER_VALUE) == 16 && _Alignof(WHV_REGISTER_VALUE) == 16);
CHECK(sizeof(WHV_VP_EXIT_CONTEXT) == 40);
CHECK(offsetof(WHV_VP_EXIT_CONTEXT, Cs) == 8);
CHECK(offsetof(WHV_VP_EXIT_CONTEXT, Rip) == 24);
CHECK(offsetof(WHV_VP_EXIT_CONTEXT, Rflags) == 32);
CHECK(offsetof(WHV_MEMORY_ACCESS_CONTEXT, AccessInfo) == 20);
CHECK(offsetof(WHV_MEMORY_ACCESS_CONTEXT, Gpa) == 24);
CHECK(offsetof(WHV_X64_IO_PORT_ACCESS_CONTEXT, PortNumber) == 24);
CHECK(offsetof(WHV_X64_IO_PORT_ACCESS_CONTEXT, Rax) == 32);
CHECK(offsetof(WHV_X64_IO_PORT_ACCESS_CONTEXT, Ds) == 64);
CHECK(sizeof(WHV_RUN_VP_EXIT_CONTEXT) == 224);
CHECK(offsetof(WHV_RUN_VP_EXIT_CONTEXT, VpContext) == 8);
CHECK(offsetof(WHV_RUN_VP_EXIT_CONTEXT, MemoryAccess) == 48);
CHECK(offsetof(WHV_RUN_VP_EXIT_CONTEXT, ApicEoi) == 48);
CHECK(sizeof(WHV_INTERRUPT_CONTROL) == 16);
CHECK(offsetof(WHV_INTERRUPT_CONTROL, Destination) == 8);
CHECK(sizeof(WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS) == 16);
CHECK(sizeof(WHV_TRANSLATE_GVA_RESULT) == 8);
CHECK(sizeof(WHV_EMULATOR_IO_ACCESS_INFO) == 12);
CHECK(offsetof(WHV_EMULATOR_IO_ACCESS_INFO, Port) == 2);
CHECK(offsetof(WHV_EMULATOR_IO_ACCESS_INFO, AccessSize) == 4);
CHECK(offsetof(WHV_EMULATOR_IO_ACCESS_INFO, Data) == 8);
CHECK(sizeof(WHV_EMULATOR_MEMORY_ACCESS_INFO) == 24);
CHECK(offsetof(WHV_EMULATOR_MEMORY_ACCESS_INFO, Direction) == 8);
CHECK(offsetof(WHV_EMULATOR_MEMORY_ACCESS_INFO, AccessSize) == 9);
CHECK(offsetof(WHV_EMULATOR_MEMORY_ACCESS_INFO, Data) == 10);
CHECK(sizeof(WHV_EMULATOR_CALLBACKS) == 48);
CHECK(WHvX64RegisterRsp == 0x04 && WHvX64RegisterRsi == 0x06);
CHECK(WHvX64RegisterRip == 0x10 && WHvX64RegisterRflags == 0x11);
CHECK(WHvX64RegisterEs == 0x12 && WHvX64RegisterCs == 0x13 && WHvX64RegisterSs == 0x14);
CHECK(WHvX64RegisterDs == 0x15 && WHvX64RegisterFs == 0x16 && WHvX64RegisterGs == 0x17);
CHECK(WHvX64RegisterLdtr == 0x18 && WHvX64RegisterTr == 0x19);
CHECK(WHvX64RegisterIdtr == 0x1a && WHvX64RegisterGdtr == 0x1b);
CHECK(WHvX64RegisterCr0 == 0x1c && WHvX64RegisterCr3 == 0x1e && WHvX64RegisterCr4 == 0x1f);
CHECK(WHvX64RegisterEfer == 0x2001);
CHECK(WHvPartitionPropertyCodeLocalApicEmulationMode == 0x1005);
CHECK(WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks == 0x100c);
CHECK(WHvPartitionPropertyCodeProcessorCount == 0x1fff);
CHECK(WHvX64LocalApicEmulationModeXApic == 1);
CHECK(WHvRunVpExitReasonMemoryAccess == 1 && WHvRunVpExitReasonX64IoPortAccess == 2);
CHECK(WHvRunVpExitReasonUnrecoverableException == 4 && WHvRunVpExitReasonInvalidVpRegisterValue == 5);
CHECK(WHvRunVpExitReasonUnsupportedFeature == 6 && WHvRunVpExitReasonX64InterruptWindow == 7);
CHECK(WHvRunVpExitReasonX64Halt == 8 && WHvRunVpExitReasonX64ApicEoi == 9);
CHECK(WHvRunVpExitReasonCanceled == 0x2001);
CHECK((WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute) == 7);
CHECK(WHvX64InterruptTypeFixed == 0 && WHvX64InterruptTypeLowestPriority == 1);
/* The synthetic feature mask, as a constant the script reads back below. */
__attribute__((used)) const WHV_SYNTHETIC_PROCESSOR_FEATURES bvmm_mask = {{
    .HypervisorPresent = 1, .Hv1 = 1, .AccessVpRunTimeReg = 1,
    .AccessPartitionReferenceCounter = 1, .AccessIntrCtrlRegs = 1,
    .AccessHypercallRegs = 1, .AccessVpIndex = 1,
    .AccessPartitionReferenceTsc = 1, .AccessFrequencyRegs = 1 }};
C
"$cc" -c -o "$dir/layout.o" "$dir/layout.c"
mask=$(x86_64-w64-mingw32-objdump -s -j .rdata "$dir/layout.o" | awk '/^ 0000 /{print $2}')
if [ "$mask" != "cf0b0000" ]; then
    echo "synthetic feature mask mismatch: header gives $mask, whp.rs uses 0xbcf" >&2
    exit 1
fi
echo "WHP layout: all sizes, offsets and constants match the Windows SDK headers"
