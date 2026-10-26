<!-- SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors -->
<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# Gaming VMs with the RTX 3080 Ti (GPU passthrough)

This page sets up the reference laptop (i9-12900H, RTX 3080 Ti Laptop GPU,
32 GB) so that bContain gives the **whole NVIDIA GPU** to a **Bazzite** VM.
Bazzite is a SteamOS-style system with Steam's Gaming Mode and NVIDIA's own
driver. Games run on the real GPU at close to native speed.

**Why Linux, and why Bazzite.** Windows cannot hand a GPU to a VM:
Windows Hypervisor Platform has no GPU feature, so this needs Linux as the
host. Valve's SteamOS ships only open-source graphics drivers, which are far
slower on NVIDIA. Bazzite's NVIDIA images give the same Gaming Mode with
NVIDIA's driver.

## What you get

- The VM owns the RTX 3080 Ti while it runs. The laptop's own desktop keeps
  running on the Intel GPU.
- Each laptop shows the picture in one of two ways:
  - **On an external monitor**, if the laptop's HDMI port is wired to the
    NVIDIA GPU (most 3080 Ti laptops with a MUX switch are).
  - **In a window via streaming**: Bazzite's built-in **Sunshine** streams
    the GPU's output. **Moonlight** on the host shows it and sends keyboard,
    mouse and controller input back. bContain forwards Sunshine's ports
    automatically, so Moonlight connects to `127.0.0.1`.
- When the VM stops, bContain gives the GPU back to the host.

## 1. Firmware settings (once)

In the laptop's BIOS/UEFI setup (usually F2 or Del at power-on):

1. **Intel Virtualization Technology (VT-x): Enabled.**
2. **VT-d: Enabled.** This is the IOMMU, which keeps a passed-through device
   from reaching the host's memory.
3. **GPU mode / MUX switch: Hybrid** (also called Optimus, MSHybrid or
   Switchable). The Intel GPU then drives the built-in screen and the NVIDIA
   GPU is free for the VM. In "Discrete only" mode the NVIDIA GPU drives the
   screen and cannot be passed through.

## 2. Linux next to Windows

Install a mainstream distribution alongside Windows. Fedora Workstation 41+
or Ubuntu 24.04 work well: shrink the Windows partition from inside Windows
first (Disk Management), then install Linux into the free space. Then:

```sh
# Turn the IOMMU on (Fedora shown; on Ubuntu edit GRUB_CMDLINE_LINUX_DEFAULT
# in /etc/default/grub and run `sudo update-grub`)
sudo grubby --update-kernel=ALL --args="intel_iommu=on iommu=pt"
sudo reboot

# QEMU, UEFI firmware and Rust
sudo dnf install qemu-kvm qemu-img edk2-ovmf          # Ubuntu: sudo apt install qemu-system-x86 qemu-utils ovmf
curl https://sh.rustup.rs -sSf | sh

# bContain
git clone -b claude/admiring-ride-ew6ayy https://github.com/inputloop/bContain
cd bContain && ./scripts/package-linux.sh             # binary in dist/bcontain-linux/
```

The NVIDIA driver on the host is optional: bContain unbinds it from the GPU
while the VM runs.

## 3. Check the machine

```sh
bcontain gpu
```

This lists the GPUs and checks everything passthrough needs: KVM, the IOMMU,
a GPU that is not driving the screen, an IOMMU group that holds only the GPU
and its audio function, and the locked-memory limit. Every failed check
comes with its fix. The app's **This machine** panel shows the same check as
**GPU passthrough**.

## 4. Create the gaming VM

1. Download **Bazzite** from bazzite.gg. Choose your hardware with an NVIDIA
   RTX GPU. The **Deck/HTPC** edition (`bazzite-deck-nvidia`) boots straight
   into Gaming Mode like SteamOS; the desktop edition starts in KDE with
   Gaming Mode one click away.
2. Start bContain with root, which passthrough needs (it uses your own
   user's VMs and settings under sudo):
   ```sh
   sudo ./bcontain
   ```
3. Pick the Bazzite ISO. bContain recognises it and switches to the
   **gaming** preset (16 vCPUs and 16 GiB on this laptop, a 256 GB disk,
   UEFI). Tick **Give the GPU to this VM** and press **Create machine**.
   From the command line:
   ```sh
   sudo bcontain create bazzite ~/Downloads/bazzite-deck-nvidia-stable.iso --gpu --disk-size 256G
   sudo bcontain start bazzite
   ```

Bazzite's installer appears in QEMU's window. Install to the VM's disk and
reboot into it.

### Valve's SteamOS instead of Bazzite

Choose SteamOS's recovery image (`steamdeck-repair-*.img.bz2`) with the GPU
option. bContain sets up the USB installer and the NVMe disk; after
**Re-image Steam Deck**, press **Eject installer**. SteamOS drives the
RTX 3080 Ti with Mesa's open NVK driver (Valve ships no NVIDIA driver), so
Gaming Mode works but is slower than Bazzite's.

## 5. Playing

- **External monitor on the NVIDIA HDMI port:** Gaming Mode appears there.
  Plug USB controllers into the host and pass them through later, or use
  Moonlight as below.
- **Streaming:** in Bazzite, enable Sunshine (`ujust setup-sunshine`, or
  Settings, then Sunshine) and set a PIN. On the host, install Moonlight
  (`flatpak install flathub com.moonlight_stream.Moonlight`), add the PC
  `127.0.0.1`, pair, and start **Desktop** or **Steam Big Picture**.

## When something is off

| Symptom | Cause and fix |
|---|---|
| `the NVIDIA GPU is in use by ...` | A program holds `/dev/nvidia*` (a desktop session on the dGPU, CUDA, `nvidia-persistenced`). Close it, or run the desktop on the Intel GPU. |
| The GPU's group contains other devices | This laptop's PCIe layout does not isolate the GPU; passthrough would have to take those devices too. bContain refuses rather than take them. |
| In the VM, `nvidia-smi` fails / `dmesg` shows `RmInitAdapter failed` | Laptop GPUs often keep their video BIOS in the system firmware, where the VM cannot reach it. Get a copy with `sudo bcontain gpu dump-vbios vbios.rom`, or extract it from the laptop's BIOS update with VBiosFinder, then `sudo bcontain gpu set-rom bazzite vbios.rom`. |
| The VM shut down by itself and the host has no NVIDIA GPU | Give it back: `sudo bcontain gpu release bazzite`. |
| "Locked memory" check fails | VFIO pins all guest RAM. Run bContain with sudo, or raise `memlock` in `/etc/security/limits.conf`. |

## What is checked, and what is not

Checked in development: GPU discovery and the readiness checks, against
fake sysfs trees modelled on an Optimus laptop (GPU plus audio behind a root
port, and a group shared with a USB controller). QEMU accepts the generated
`vfio-pci` and `pcie-root-port` options, and the Sunshine UDP/TCP forwards.
UEFI boots with a per-VM variable store that keeps the boot entries.

Not yet checked: a real passthrough on the laptop. That needs Linux on it,
and it is the next test.
