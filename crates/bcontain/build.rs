// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Embed a QEMU build (a .tar.gz of the QEMU folder) into the executable when
//! `BCONTAIN_EMBED_QEMU` points at one. This is what makes the Windows build a
//! single file: scripts/bundle-qemu-windows.ps1 sets it.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(embedded_qemu)");
    println!("cargo:rerun-if-env-changed=BCONTAIN_EMBED_QEMU");
    if let Some(p) = std::env::var_os("BCONTAIN_EMBED_QEMU").filter(|p| !p.is_empty()) {
        let p =
            std::fs::canonicalize(&p).expect("BCONTAIN_EMBED_QEMU must name an existing .tar.gz");
        let len = std::fs::metadata(&p).expect("reading QEMU payload").len();
        println!("cargo:rerun-if-changed={}", p.display());
        println!("cargo:rustc-cfg=embedded_qemu");
        println!("cargo:rustc-env=BCONTAIN_QEMU_PAYLOAD={}", p.display());
        println!("cargo:rustc-env=BCONTAIN_QEMU_PAYLOAD_LEN={len}");
    }
}
