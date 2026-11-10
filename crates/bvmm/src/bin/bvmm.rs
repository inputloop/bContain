// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! `bvmm`: run a Linux kernel on bContain's own hypervisor, with nothing
//! else around it. For bring-up, tests and embedding experiments; bContain
//! itself uses the library.

use std::path::PathBuf;
use std::process::ExitCode;

use bvmm::platform::ClockSource;
use bvmm::{DiskConfig, Outcome, VmConfig};

const USAGE: &str = "\
usage: bvmm run --kernel <bzImage> [options]
       bvmm self-test

options for run:
  --initrd <file>        initial ramdisk
  --cmdline <text>       kernel command line
  --cmdline-file <file>  kernel command line read from a file (newlines become spaces)
  --disk <path>[,ro]     raw image or disk layout; repeat for more (first is vda)
  --cpus <n>             virtual processors (default 1)
  --memory <MiB>         guest memory (default 1024)
  --clock host-tsc       experimental, WHP: give the guest the measured TSC frequency
  --display <W>x<H>      give the guest a screen of this size
  --screenshot <file>    with --display: keep the newest frame in this PPM file,
                         rewritten about once a second while the guest draws
  --touchscreen          with --display: a multi-touch screen of the same size
  --keyboard             a keyboard
  --stdin                send this process's standard input to the guest console

The guest's serial console is written to standard output.
exit status: 0 the guest reset (how a Linux guest normally ends here),
             2 the guest halted, 1 an error";

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

struct Options {
    cfg: VmConfig,
    stdin: bool,
    screenshot: Option<PathBuf>,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut cfg = VmConfig {
        memory_mib: 1024,
        cpus: 1,
        ..Default::default()
    };
    let mut kernel: Option<PathBuf> = None;
    let mut initrd: Option<PathBuf> = None;
    let mut stdin = false;
    let mut screenshot: Option<PathBuf> = None;
    let (mut touchscreen, mut keyboard) = (false, false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--kernel" => kernel = Some(value(&mut args, &a)?.into()),
            "--initrd" => initrd = Some(value(&mut args, &a)?.into()),
            "--cmdline" => cfg.cmdline = value(&mut args, &a)?,
            "--cmdline-file" => {
                let path = value(&mut args, &a)?;
                let text =
                    std::fs::read_to_string(&path).map_err(|e| format!("reading {path}: {e}"))?;
                cfg.cmdline = text.split_whitespace().collect::<Vec<_>>().join(" ");
            }
            "--disk" => {
                let v = value(&mut args, &a)?;
                let (path, read_only) = match v.strip_suffix(",ro") {
                    Some(p) => (p.to_string(), true),
                    None => (v, false),
                };
                cfg.disks.push(DiskConfig {
                    path: path.into(),
                    read_only,
                });
            }
            "--cpus" => {
                cfg.cpus = value(&mut args, &a)?
                    .parse()
                    .map_err(|_| "--cpus needs a number from 1 to 255".to_string())?
            }
            "--memory" => {
                cfg.memory_mib = value(&mut args, &a)?
                    .parse()
                    .map_err(|_| "--memory needs a number of MiB".to_string())?
            }
            "--clock" => match value(&mut args, &a)?.as_str() {
                "paravirtual" => cfg.clock = ClockSource::Paravirtual,
                "host-tsc" => cfg.clock = ClockSource::HostTsc,
                other => return Err(format!("unknown clock `{other}`")),
            },
            "--display" => {
                let v = value(&mut args, &a)?;
                let size = v
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
                    .filter(|(w, h)| *w > 0 && *h > 0)
                    .ok_or("--display needs a size such as 720x1280")?;
                cfg.display = Some(bvmm::virtio::gpu::Display::new(size.0, size.1));
            }
            "--screenshot" => screenshot = Some(value(&mut args, &a)?.into()),
            "--touchscreen" => touchscreen = true,
            "--keyboard" => keyboard = true,
            "--stdin" => stdin = true,
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    let kernel = kernel.ok_or("--kernel is required")?;
    cfg.kernel =
        std::fs::read(&kernel).map_err(|e| format!("reading {}: {e}", kernel.display()))?;
    if let Some(p) = initrd {
        cfg.initrd = Some(std::fs::read(&p).map_err(|e| format!("reading {}: {e}", p.display()))?);
    }
    if screenshot.is_some() && cfg.display.is_none() {
        return Err("--screenshot needs --display".into());
    }
    // The senders are dropped: this command has no way to make input yet.
    // The devices are still worth having, so the guest sees them.
    if touchscreen {
        let (w, h) = cfg
            .display
            .as_ref()
            .ok_or("--touchscreen needs --display")?
            .size();
        cfg.inputs
            .push(bvmm::virtio::input::Input::touchscreen(w, h).0);
    }
    if keyboard {
        cfg.inputs.push(bvmm::virtio::input::Input::keyboard().0);
    }
    Ok(Options {
        cfg,
        stdin,
        screenshot,
    })
}

/// Keep `path` holding the newest frame while the guest runs.
fn save_frames(display: bvmm::virtio::gpu::Display, path: PathBuf) {
    std::thread::spawn(move || {
        let mut saved = 0;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if display.sequence() == saved {
                continue;
            }
            let frame = display.frame();
            saved = frame.sequence;
            // Write beside the target and rename, so a reader never sees half a file.
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, frame.to_ppm()).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    });
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("self-test") => match bvmm::self_test(&mut |step| println!("  - {step}")) {
            Ok(()) => {
                println!("PASS");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("FAIL: {e}");
                ExitCode::FAILURE
            }
        },
        Some("run") => {
            let Options {
                cfg,
                stdin,
                screenshot,
            } = match parse(args) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("bvmm: {e}\n\n{USAGE}");
                    return ExitCode::FAILURE;
                }
            };
            if let (Some(display), Some(path)) = (&cfg.display, screenshot) {
                save_frames(display.clone(), path);
            }
            let input: Option<Box<dyn std::io::Read + Send>> =
                stdin.then(|| Box::new(std::io::stdin()) as _);
            match bvmm::run_linux(&cfg, Box::new(std::io::stdout()), input) {
                Ok(Outcome::Reset) => ExitCode::SUCCESS,
                Ok(Outcome::Halted) => {
                    eprintln!("bvmm: the guest halted");
                    ExitCode::from(2)
                }
                Err(e) => {
                    eprintln!("bvmm: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> impl Iterator<Item = String> {
        s.iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parses_a_full_command_line() {
        let dir = std::env::temp_dir().join(format!("bvmm-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let k = dir.join("k");
        let c = dir.join("c");
        std::fs::write(&k, b"kernel").unwrap();
        std::fs::write(&c, "console=ttyS0\n  init=/init\n").unwrap();
        let Options { cfg, stdin, .. } = parse(args(&[
            "--kernel",
            k.to_str().unwrap(),
            "--cmdline-file",
            c.to_str().unwrap(),
            "--disk",
            "a.img",
            "--disk",
            "b.layout,ro",
            "--cpus",
            "4",
            "--memory",
            "4096",
            "--clock",
            "host-tsc",
            "--display",
            "720x1280",
            "--touchscreen",
            "--keyboard",
            "--stdin",
        ]))
        .unwrap();
        assert_eq!(cfg.kernel, b"kernel");
        assert_eq!(cfg.cmdline, "console=ttyS0 init=/init");
        assert_eq!((cfg.cpus, cfg.memory_mib, stdin), (4, 4096, true));
        assert_eq!(cfg.clock, ClockSource::HostTsc);
        assert_eq!(cfg.disks.len(), 2);
        assert!(!cfg.disks[0].read_only && cfg.disks[1].read_only);
        assert_eq!(cfg.disks[1].path.to_str(), Some("b.layout"));
        assert_eq!(cfg.display.unwrap().size(), (720, 1280));
        assert_eq!(cfg.inputs.len(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_bad_command_lines() {
        assert!(parse(args(&[])).err().unwrap().contains("--kernel"));
        assert!(parse(args(&["--cpus"]))
            .err()
            .unwrap()
            .contains("needs a value"));
        assert!(parse(args(&["--frobnicate"]))
            .err()
            .unwrap()
            .contains("unknown option"));
        assert!(parse(args(&["--clock", "sundial"])).is_err());
        assert!(parse(args(&["--display", "720"])).is_err());
        assert!(parse(args(&["--display", "0x5"])).is_err());
        assert!(
            parse(args(&["--touchscreen"])).is_err(),
            "no kernel, no display"
        );
    }
}
