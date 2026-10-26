#!/bin/sh
# Package bContain for Linux: dist/bcontain-linux-x86_64.tar.gz
#
# The binary is self-contained. QEMU comes from the distribution, which is
# the reliable way to get KVM, virgl and OVMF builds that match your drivers:
#   Debian/Ubuntu: sudo apt install qemu-system-x86 qemu-utils ovmf
#   Fedora:        sudo dnf install qemu-kvm qemu-img edk2-ovmf
#   Arch:          sudo pacman -S qemu-desktop edk2-ovmf
# A QEMU build placed in dist/bcontain-linux/qemu/ is picked up first.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cargo build --release --manifest-path "$root/Cargo.toml"
dist="$root/dist/bcontain-linux"
rm -rf "$dist" && mkdir -p "$dist"
cp "$root/target/release/bcontain" "$root/README.md" "$dist/"
tar -C "$root/dist" -czf "$root/dist/bcontain-linux-x86_64.tar.gz" bcontain-linux
echo "wrote dist/bcontain-linux-x86_64.tar.gz"
