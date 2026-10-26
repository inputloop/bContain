// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Windows 10 and 11 guests: the answer-file disc that lets Windows 11 Setup
//! run without a TPM or Secure Boot.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// Windows Setup looks for `autounattend.xml` at the root of every removable
/// drive, CDs included. This one only sets Setup's documented LabConfig
/// switches, which skip Windows 11's hardware checks (TPM 2.0, Secure Boot,
/// RAM, CPU, storage) - the same as Rufus's "remove requirements" option.
/// Every other screen of Setup is left to the person installing; Windows 10
/// ignores the switches.
pub const ANSWER_FILE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend">
  <settings pass="windowsPE">
    <component name="Microsoft-Windows-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <RunSynchronous>
        <RunSynchronousCommand wcm:action="add">
          <Order>1</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassTPMCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>2</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassSecureBootCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>3</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassRAMCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>4</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassCPUCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>5</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassStorageCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
      </RunSynchronous>
    </component>
  </settings>
</unattend>
"#;

/// Write the answer-file disc into the VM's folder.
pub fn answer_iso(vm_dir: &Path) -> Result<PathBuf> {
    let path = vm_dir.join("autounattend.iso");
    crate::iso::write(
        &path,
        "BCONTAIN_ANSWERS",
        &[("autounattend.xml", ANSWER_FILE.as_bytes())],
    )?;
    Ok(path)
}

/// A system disk Windows has not been installed on yet: an empty qcow2 is a
/// few hundred KiB, an installed Windows several GiB.
pub fn disk_is_fresh(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() < 16 * 1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_file_is_well_formed() {
        // Balanced tags, and every switch present.
        assert_eq!(
            ANSWER_FILE.matches("<RunSynchronousCommand").count(),
            ANSWER_FILE.matches("</RunSynchronousCommand>").count()
        );
        for k in ["TPM", "SecureBoot", "RAM", "CPU", "Storage"] {
            assert!(ANSWER_FILE.contains(&format!("/v Bypass{k}Check ")));
        }
    }
}
