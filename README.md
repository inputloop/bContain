# bContain

A standalone runner for **virtual machines and containers** on **Windows and
Linux**. It is tuned for an **RTX 3080 Ti / Core i9 / 32 GB laptop, or better**.

- **Boots disk images:** ISO, qcow2, raw `.img`, VHDX, VHD, VMDK, VDI. The format
  is detected from the file's contents, not its extension.
- **Runs container images:** `docker save` tarballs, OCI layouts, or images
  pulled straight from Docker Hub, GHCR or any v2 registry. Docker is not
  required. Each container runs in its own lightweight VM.
- **Hardware-accelerated:** WHPX (Windows Hypervisor Platform) on Windows and
  KVM on Linux, through a bundled QEMU. It falls back to software emulation
  when neither is available.
- **Tuned presets** sized from the detected host. Guests get virtio-gpu with
  virgl 3D on the NVIDIA GPU, and hugepages when they are reserved.
- **One binary** with both a CLI and a local web dashboard.
- **Ready for its own hypervisor:** every engine call goes through a
  `Hypervisor` trait, and `crates/bvmm` is where the native VMM is being built
  (see [docs/NATIVE-VMM.md](docs/NATIVE-VMM.md)).

![Dashboard (dark)](docs/screenshots/dashboard-dark.png)

> The screenshots are real output, captured in a cloud sandbox with a 4-thread
> Xeon, no GPU and no KVM. That is why `doctor` flags everything there and
> shows the fixes it would suggest. On the reference laptop those rows are
> green.

## Quick start

```sh
bcontain doctor                     # check this machine against the reference build
bcontain kernel fetch               # one-time: guest kernel for container images

bcontain run alpine:3.20 -- uname -a           # one-shot container, removed afterwards
bcontain run ubuntu-24.04.iso                  # boot an ISO live (writes discarded)

bcontain create dev ubuntu-24.04.iso           # persistent VM; adds a 64 GiB qcow2 disk
bcontain start dev
bcontain create web nginx:alpine -p 8080:80    # persistent container, port forwarded
bcontain start web
bcontain list
bcontain ui --open                             # web dashboard on http://127.0.0.1:7878
```

![bcontain run](docs/screenshots/cli-run-container.png)

## Hardware check: `bcontain doctor`

`doctor` compares the host with the reference build and tells you exactly
what to change: CPU class, RAM, NVIDIA GPU and VRAM (via `nvidia-smi`), the
hardware accelerator, QEMU, the container kernel, and hugepages.

![bcontain doctor](docs/screenshots/cli-doctor.png)

"Or better" is built in. Presets are computed from the machine bContain
actually runs on, so a 24-core / 64 GB workstation gets bigger guests while
the host always keeps 6 GiB of RAM and some cores for itself.

| Preset | vCPU | RAM | Display | Used for |
|---|---|---|---|---|
| `rtx3080ti` (default for VMs) | half the threads, max 16 | min(16 GiB, spare) | virgl 3D | desktop guests |
| `light` | 4 | 4 GiB | basic | background VMs |
| `max` | threads − 2 | all spare RAM | virgl 3D | heavy workloads |
| `container` (default for images) | 4 | 2 GiB (grows to fit rootfs) | headless | containers |

On the reference i9-12900HK (14 cores / 20 threads, 32 GB), `rtx3080ti` gives
**10 vCPU and 16 GiB**. That figure is covered by a unit test.

![bcontain presets](docs/screenshots/cli-presets.png)

## Web dashboard: `bcontain ui`

Lists, creates, starts, stops and deletes machines, and shows their logs. It
binds to `127.0.0.1` only. State-changing requests need an `X-Bcontain`
header, so other websites cannot control your VMs through your browser.

| Light | Mobile |
|---|---|
| ![Dashboard light](docs/screenshots/dashboard-light.png) | ![Dashboard mobile](docs/screenshots/dashboard-mobile.png) |

## Installing

### Windows 10/11

1. Enable **Windows Hypervisor Platform** (needs admin, then a reboot):
   ```powershell
   Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
   ```
2. Unzip `bcontain-windows.zip`. It contains `bcontain.exe` with QEMU in a
   `qemu\` folder beside it, so nothing else needs installing.
3. Run `bcontain doctor`.

To build the zip yourself: `cargo build --release`, then
`.\scripts\bundle-qemu-windows.ps1`. This downloads the official QEMU Windows
build and unpacks it with 7-Zip.

### Linux

```sh
sudo apt install qemu-system-x86 qemu-utils ovmf      # Fedora: qemu-kvm qemu-img edk2-ovmf
sudo usermod -aG kvm $USER                            # then log out and back in
./scripts/package-linux.sh                            # or: cargo build --release
```

QEMU comes from your distribution, which is the reliable way to get KVM,
virgl and OVMF builds that match your drivers. A QEMU placed in a `qemu/`
folder next to the binary takes priority, as on Windows.

### Where QEMU and the kernel are found

| What | Search order |
|---|---|
| QEMU | `$BCONTAIN_QEMU` → `qemu/` next to `bcontain` → `C:\Program Files\qemu` → `PATH` |
| Container kernel | `--kernel` → `$BCONTAIN_KERNEL` → `kernel/vmlinuz` next to `bcontain` → `<data>/kernel/vmlinuz` |
| Data | `$BCONTAIN_HOME` → `%LOCALAPPDATA%\bcontain` / `~/.local/share/bcontain` |

## Tuning for the 3080 Ti laptop

- **GPU:** guests get `virtio-vga-gl` with virgl, so OpenGL is rendered by the
  host's NVIDIA driver. On hybrid-graphics laptops, set the MUX to
  dGPU / "Discrete only" (or use Advanced Optimus) so QEMU's window renders on
  the 3080 Ti rather than the iGPU. Virgl is the default on Linux. On Windows
  the default is `basic`, because GL support varies between QEMU Windows
  builds; pass `--display virgl` to opt in.
- **Hugepages (Linux):** `sudo sysctl vm.nr_hugepages=8192` reserves 16 GiB.
  bContain then backs guest RAM with hugepages automatically, and only when
  the pool can hold the guest.
- **CPU:** `-cpu host` under KVM, and `-cpu max` under WHPX
  (with `kernel-irqchip=off` for stability across Windows builds).
- **Windows guests:** use `--legacy-devices` (SATA disk + e1000e NIC) for stock
  Windows installers, which ship without virtio drivers.

## Command reference

| Command | What it does |
|---|---|
| `doctor [--json]` | Check the host against the reference build |
| `presets` | Show each preset's sizing on this host |
| `create NAME IMAGE [opts] [-- CMD…]` | Define a VM or container. Creating from an ISO adds a blank disk (`--disk-size`, default 64G) |
| `start NAME [--attach]` | Boot in the background, or attached to the serial console |
| `stop NAME [--force]` | ACPI shutdown, or pull the plug |
| `list` · `info NAME` · `logs NAME` | Inspect. `info` prints the exact QEMU command line |
| `remove NAME [--force]` | Delete the definition and the disks bContain created. **Your source image is never touched** |
| `run IMAGE [opts] [-- CMD…]` | One-shot run attached to the terminal. Disk writes are discarded unless you pass `--persist` |
| `pull REF` · `images` | Download and list registry images |
| `kernel fetch / install PATH / path` | Manage the guest kernel for containers |
| `ui [--port N] [--open]` | Web dashboard |

Options shared by `create` and `run`: `--preset`, `--cpus`, `--memory 16G`,
`--display virgl|basic|headless`, `--firmware bios|uefi`, `--legacy-devices`,
`-p HOST:GUEST`, `-e KEY=VALUE`, `--kernel`.

## How containers run

1. The image is resolved: a local `docker save` tar, an OCI layout, or a
   registry pull (linux/amd64, digests verified).
2. Layers are flattened **in memory**, applying OCI whiteouts and opaque
   directories. Doing this in memory keeps Unix modes, owners and symlinks
   intact even on Windows, whose filesystem cannot represent them.
3. The tree is written as a gzip `newc` initramfs, plus `/.bcontain/init`. That
   script mounts `/proc`, `/sys` and `/dev`, sets up networking
   (10.0.2.15, with DNS via QEMU's 10.0.2.3), exports the image's `Env`, `cd`s to
   `WorkingDir`, and runs `Entrypoint + Cmd`.
4. QEMU boots the guest kernel with that initramfs. The console is on serial,
   and when the command exits, the VM powers off.

Current limits: the image must contain `/bin/sh`, so distroless and scratch
images are not supported yet. Commands run as root inside their VM (`USER` is
reported and ignored). There are no volume mounts yet. Zstd-compressed layers
are not read yet. Registry pulls are anonymous, so public images only.

## Layout

```
crates/bcontain/src/
  main.rs      CLI
  engine.rs    VmSpec + Hypervisor trait (the backend seam)
  qemu.rs      QEMU backend: WHPX/KVM/TCG, virgl, UEFI, QMP control
  image.rs     disk/OCI format detection by magic bytes
  oci.rs       layer flattening, initramfs (cpio) writer, guest init
  registry.rs  OCI distribution client (anonymous bearer auth)
  host.rs      hardware detection + reference-build checks
  preset.rs    host-relative sizing presets
  store.rs     per-VM state on disk
  ops.rs       operations shared by CLI and web UI
  web.rs, ui/  local dashboard
crates/bvmm/   native-VMM seam: accelerator probing, future hypervisor
scripts/       Windows QEMU bundler, Linux packager, screenshot generator
```

## Development

```sh
cargo test --workspace                       # unit tests
cargo build --target x86_64-pc-windows-gnu   # cross-check the Windows build from Linux
node scripts/screenshots.js target/debug/bcontain   # regenerate docs/screenshots (needs `bcontain ui` running)
```

What has been verified, and where:

- **In the Linux sandbox:** all tests pass, and the Windows target
  cross-compiles. A two-layer image with a whiteout booted end to end and ran
  its command (shown above). The full create → start → list → logs → stop
  (ACPI and forced) → remove lifecycle works, as does creating an ISO VM with
  a blank qcow2 disk. The dashboard API was exercised, including its
  cross-site guard.
- **Not yet verified on real hardware:** the WHPX path on Windows, virgl on an
  NVIDIA GPU, registry pulls (the sandbox's registry traffic was rate-limited
  or blocked; the auth handshake itself completed), and the Windows bundling
  script and CI workflow.
