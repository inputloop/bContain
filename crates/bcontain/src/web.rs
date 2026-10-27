// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Local web dashboard: `bcontain ui` serves it on 127.0.0.1 only.
//!
//! State-changing requests must carry `X-Bcontain: 1`. Browsers will not send
//! a custom header cross-origin without a CORS preflight (which this server
//! never approves), so other websites cannot drive your VMs.

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::host::Host;
use crate::ops;
use crate::preset::Preset;
use crate::store;

const INDEX: &str = include_str!("ui/index.html");
use crate::KERNEL_IMAGE;

fn looks_like_gaming(path: &std::path::Path) -> bool {
    let n = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    crate::ops::is_steamos_image(path)
        || ["bazzite", "steamos", "holoiso", "chimeraos", "nobara"]
            .iter()
            .any(|k| n.contains(k))
}

fn looks_like_windows(path: &std::path::Path) -> bool {
    crate::image::is_windows_installer(path)
}

/// GPU passthrough readiness for the "This machine" panel.
fn gpu_status() -> Value {
    let sys = crate::gpu::Sysfs::default();
    let candidate = sys.passthrough_candidate();
    let checks = crate::gpu::readiness(&sys, candidate.as_ref());
    json!({
        "ready": checks.iter().all(|c| c.ok),
        "linux": cfg!(target_os = "linux"),
        "gpu": candidate.map(|g| g.name),
        "checks": checks,
    })
}

/// What the GUI shows about a chosen image before creating a machine from it.
fn inspect(image: &str) -> Value {
    let path = std::path::Path::new(image);
    let name = crate::desktop::suggest_name(image);
    if image.is_empty() {
        return json!({ "ok": false, "message": "" });
    }
    if path.exists() {
        return match crate::desktop::describe(path) {
            Some(kind) => {
                let size = std::fs::metadata(path)
                    .map(|m| m.len() / 1048576)
                    .unwrap_or(0);
                json!({
                    "ok": true,
                    "kind": kind,
                    "size_mib": size,
                    "container": kind == "container image",
                    // SteamOS installs onto a new disk, like an ISO does;
                    // alphaOS runs live and needs none.
                    "iso": kind.starts_with("ISO") || crate::ops::is_steamos_image(path),
                    "alphaos": crate::image::is_alphaos_iso(path),
                    "name": name,
                    // Stock Windows media has no virtio drivers; suggest SATA + e1000e.
                    "windows": looks_like_windows(path),
                    // Gaming systems: suggest the gaming preset and GPU passthrough.
                    "gaming": looks_like_gaming(path),
                })
            }
            None => {
                json!({ "ok": false, "message": "not a disk or container image bContain can run" })
            }
        };
    }
    match crate::registry::Reference::parse(image) {
        Ok(r) if !crate::ops::looks_like_path(image) && !image.contains(' ') => json!({
            "ok": true,
            "kind": format!("container image from {}", r.registry.replace("registry-1.docker.io", "Docker Hub")),
            "container": true,
            "iso": false,
            "remote": true,
            "name": name,
        }),
        _ => json!({ "ok": false, "message": "file not found" }),
    }
}

fn bind(port: u16) -> Result<(Server, String)> {
    let addr = format!("127.0.0.1:{port}");
    let server = Server::http(&addr).map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))?;
    let port = server.server_addr().to_ip().map_or(port, |a| a.port());
    Ok((server, format!("http://127.0.0.1:{port}/")))
}

fn run(server: Server, port: u16) {
    let host = Host::detect();
    for req in server.incoming_requests() {
        let host = host.clone();
        std::thread::spawn(move || handle(req, &host, port));
    }
}

/// `bcontain ui`: serve the dashboard until Ctrl+C.
pub fn serve(port: u16, open: bool) -> Result<()> {
    let (server, url) = bind(port)?;
    println!("bContain dashboard on {url}  (Ctrl+C to stop)");
    if open {
        crate::desktop::open_app_window(&url);
    }
    run(server, port_of(&url));
    Ok(())
}

/// Double-click mode: open the dashboard in its own app window and quit when
/// that window closes.
pub fn app() -> Result<()> {
    // Prefer the usual port so the browser profile keeps its zoom etc.; any free one will do.
    let (server, url) = bind(7878).or_else(|_| bind(0))?;
    let port = port_of(&url);
    std::thread::spawn(move || run(server, port));
    println!("bContain is running at {url}");
    match crate::desktop::open_app_window(&url) {
        Some(mut window) => {
            crate::desktop::release_console_if_owned();
            let _ = window.wait();
            // The browser may have handed the window to an instance that was
            // already running, so the process ending proves nothing. The page
            // polls every few seconds; quit once it has been quiet for a while.
            // VMs keep running: they are separate processes.
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                if store::now().saturating_sub(LAST_SEEN.load(Ordering::Relaxed)) > 15 {
                    return Ok(());
                }
            }
        }
        None => {
            println!("Close this window to quit bContain.");
            loop {
                std::thread::park();
            }
        }
    }
}

/// Unix time of the last request; the app quits when its page goes quiet.
static LAST_SEEN: AtomicU64 = AtomicU64::new(0);

fn port_of(url: &str) -> u16 {
    url.rsplit(':')
        .next()
        .and_then(|p| p.trim_end_matches('/').parse().ok())
        .unwrap_or(7878)
}

fn handle(mut req: Request, host: &Host, port: u16) {
    LAST_SEEN.store(store::now(), Ordering::Relaxed);
    let method = req.method().clone();
    // Reject requests addressed to any other host name: stops DNS-rebinding
    // pages from reading the API through a name that resolves to 127.0.0.1.
    let host_ok = req.headers().iter().any(|h| {
        h.field.equiv("Host")
            && [format!("127.0.0.1:{port}"), format!("localhost:{port}")]
                .contains(&h.value.as_str().to_string())
    });
    if !host_ok {
        let _ = req.respond(Response::from_string("bad host").with_status_code(421));
        return;
    }
    let url = req.url().split('?').next().unwrap_or("/").to_string();
    let mutating = !matches!(method, Method::Get | Method::Head);
    let authorized = req
        .headers()
        .iter()
        .any(|h| h.field.equiv("X-Bcontain") && h.value.as_str() == "1");

    let result = if mutating && !authorized {
        Err((403, "missing X-Bcontain header".to_string()))
    } else {
        route(&mut req, &method, &url, host)
    };

    let resp = match result {
        Ok(Reply::Html(s)) => {
            Response::from_string(s).with_header(header("Content-Type", "text/html; charset=utf-8"))
        }
        Ok(Reply::Text(s)) => Response::from_string(s)
            .with_header(header("Content-Type", "text/plain; charset=utf-8")),
        Ok(Reply::Json(v)) => Response::from_string(v.to_string())
            .with_header(header("Content-Type", "application/json")),
        Err((code, msg)) => Response::from_string(json!({ "error": msg }).to_string())
            .with_status_code(code)
            .with_header(header("Content-Type", "application/json")),
    };
    let _ = req.respond(resp.with_header(header("Cache-Control", "no-store")));
}

enum Reply {
    Html(String),
    Text(String),
    Json(Value),
}

type Routed = std::result::Result<Reply, (u16, String)>;

fn route(req: &mut Request, method: &Method, url: &str, host: &Host) -> Routed {
    let parts: Vec<&str> = url.trim_matches('/').split('/').collect();
    match (method, parts.as_slice()) {
        (Method::Get, [""]) => Ok(Reply::Html(INDEX.to_string())),
        (Method::Get, ["api", "host"]) => {
            let presets: Vec<Value> = Preset::ALL
                .iter()
                .map(|p| json!({ "name": p.name(), "sizing": p.sizing(host) }))
                .collect();
            let qemu = crate::qemu::Qemu::locate().ok();
            let probe = bvmm::probe();
            let native = json!({ "accel": probe.accel.to_string(), "usable": probe.usable });
            Ok(Reply::Json(json!({
                "host": host,
                "checks": host.checks(),
                "meets_baseline": host.meets_baseline(),
                "presets": presets,
                "qemu": qemu.as_ref().map(|q| json!({
                    "path": q.binary.display().to_string(),
                    "version": q.version(),
                    "accel": q.accel.qemu_name(),
                })),
                "kernel": crate::oci::find_kernel(None).ok().map(|p| p.display().to_string()),
                "data_dir": store::data_dir().display().to_string(),
                "bundled_qemu": crate::qemu::has_embedded_qemu(),
                "native": native,
                "gpu": gpu_status(),
                "os": std::env::consts::OS,
            })))
        }
        // Native "open file" dialog. POST so only our page can trigger it.
        (Method::Post, ["api", "pick"]) => {
            let path = crate::desktop::pick_file().map_err(bad)?;
            let info = path.as_deref().map(inspect);
            Ok(Reply::Json(json!({ "path": path, "info": info })))
        }
        (Method::Post, ["api", "pick-folder"]) => {
            let path = crate::desktop::pick_folder().map_err(bad)?;
            Ok(Reply::Json(json!({ "path": path })))
        }
        // Free space on the drive a VM's disks would go to ("" = the default).
        (Method::Post, ["api", "space"]) => {
            let body: InspectBody = read_json(req)?;
            let path = body.image.trim();
            let target = if path.is_empty() {
                store::data_dir().join("vms")
            } else {
                std::path::PathBuf::from(path)
            };
            if !target.is_absolute() {
                return Ok(Reply::Json(
                    json!({ "ok": false, "message": "the storage folder must be a full path, e.g. D:\\VMs" }),
                ));
            }
            Ok(Reply::Json(match store::free_space(&target) {
                Some((free, total, drive)) => json!({
                    "ok": true,
                    "default": store::data_dir().join("vms").display().to_string(),
                    "drive": drive.display().to_string(),
                    "free_mib": free >> 20,
                    "total_mib": total >> 20,
                }),
                None => json!({ "ok": false }),
            }))
        }
        (Method::Post, ["api", "scan"]) => Ok(Reply::Json(json!(crate::desktop::scan()))),
        (Method::Post, ["api", "inspect"]) => {
            let body: InspectBody = read_json(req)?;
            Ok(Reply::Json(inspect(body.image.trim())))
        }
        (Method::Post, ["api", "hypervisor-test"]) => {
            let mut steps = Vec::new();
            let result = bvmm::self_test(&mut |s| steps.push(s.to_string()));
            Ok(Reply::Json(json!({
                "ok": result.is_ok(),
                "steps": steps,
                "error": result.err().map(|e| e.to_string()),
            })))
        }
        (Method::Post, ["api", "kernel", "fetch"]) => {
            let mut lines = Vec::new();
            let path = ops::fetch_kernel(KERNEL_IMAGE, &mut |l| lines.push(l)).map_err(bad)?;
            Ok(Reply::Json(
                json!({ "path": path.display().to_string(), "log": lines }),
            ))
        }
        (Method::Get, ["api", "vms"]) => Ok(Reply::Json(json!(ops::views().map_err(internal)?))),
        (Method::Post, ["api", "vms"]) => {
            let body: CreateBody = read_json(req)?;
            let opts = ops::CreateOptions {
                name: body.name,
                image: body.image,
                preset: body.preset.filter(|p| !p.is_empty()),
                cpus: body.cpus,
                memory_mib: body
                    .memory
                    .as_deref()
                    .filter(|m| !m.is_empty())
                    .map(ops::parse_mib)
                    .transpose()
                    .map_err(bad)?,
                ports: body
                    .ports
                    .iter()
                    .filter(|p| !p.is_empty())
                    .map(|p| ops::parse_port(p))
                    .collect::<Result<_>>()
                    .map_err(bad)?,
                legacy_devices: body.legacy_devices,
                windows: body.windows,
                install: body.install,
                storage: body
                    .storage
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(std::path::PathBuf::from),
                gpu: body.gpu,
                engine: body.engine.filter(|e| !e.is_empty()),
                accel_mode: body.accel_mode,
                disk_mib: body
                    .disk_size
                    .as_deref()
                    .filter(|m| !m.is_empty())
                    .map(ops::parse_mib)
                    .transpose()
                    .map_err(bad)?,
                ..Default::default()
            };
            let mut lines = Vec::new();
            let rec = ops::create(&opts, host, true, &mut |l| lines.push(l)).map_err(bad)?;
            let mut started = false;
            if body.start {
                match ops::start(&rec.spec.name) {
                    Ok(_) => started = true,
                    Err(e) => lines.push(format!("created, but starting failed: {e:#}")),
                }
            }
            Ok(Reply::Json(
                json!({ "name": rec.spec.name, "log": lines, "started": started }),
            ))
        }
        (Method::Post, ["api", "vms", name, "diagnose"]) => {
            store::load(name).map_err(|e| (404, format!("{e:#}")))?;
            crate::diagnose::start_background(name);
            Ok(Reply::Json(json!({ "ok": true })))
        }
        (Method::Get, ["api", "vms", name, "diagnose"]) => {
            Ok(Reply::Json(json!(crate::diagnose::status(name))))
        }
        (Method::Get, ["api", "vms", name, "settings"]) => Ok(Reply::Json(json!(
            crate::settings::details(name, host).map_err(|e| (404, format!("{e:#}")))?
        ))),
        (Method::Post, ["api", "vms", name, "settings"]) => {
            let patch: crate::settings::Patch = read_json(req)?;
            crate::settings::apply(name, host, &patch).map_err(bad)?;
            Ok(Reply::Json(json!({ "ok": true })))
        }
        (Method::Post, ["api", "vms", name, "resize"]) => {
            let body: SizeBody = read_json(req)?;
            let mib = ops::parse_mib(&body.size).map_err(bad)?;
            let (from, to) = crate::settings::resize(name, mib).map_err(bad)?;
            Ok(Reply::Json(
                json!({ "ok": true, "from_mib": from, "to_mib": to }),
            ))
        }
        (Method::Post, ["api", "vms", name, "snapshot"]) => {
            let body: SnapshotBody = read_json(req)?;
            let n = body.name.trim();
            match body.action.as_str() {
                "take" => crate::settings::snapshot_create(name, n),
                "restore" => crate::settings::snapshot_restore(name, n),
                "delete" => crate::settings::snapshot_delete(name, n),
                other => return Err((400, format!("unknown snapshot action {other}"))),
            }
            .map_err(bad)?;
            Ok(Reply::Json(json!({ "ok": true })))
        }
        (Method::Post, ["api", "vms", name, "eject"]) => {
            let n = ops::eject(name).map_err(bad)?;
            Ok(Reply::Json(json!({ "ok": true, "ejected": n })))
        }
        (Method::Post, ["api", "vms", name, "accel", mode]) => {
            let mode: crate::engine::AccelMode = serde_json::from_value(json!(mode))
                .map_err(|_| (400, format!("unknown mode {mode}")))?;
            ops::set_accel_mode(name, mode).map_err(bad)?;
            Ok(Reply::Json(json!({ "ok": true })))
        }
        (Method::Post, ["api", "vms", name, action]) => {
            match *action {
                "start" => ops::start(name).map(|_| ()),
                "stop" => ops::stop(name, false),
                "kill" => ops::stop(name, true),
                _ => return Err((404, format!("unknown action {action}"))),
            }
            .map_err(bad)?;
            Ok(Reply::Json(json!({ "ok": true })))
        }
        (Method::Delete, ["api", "vms", name]) => {
            ops::remove(name, false).map_err(bad)?;
            Ok(Reply::Json(json!({ "ok": true })))
        }
        (Method::Get, ["api", "vms", name, "log"]) => {
            store::load(name).map_err(|e| (404, format!("{e:#}")))?;
            let text = crate::qemu::tail(&store::log_path(name), 200);
            Ok(Reply::Text(text))
        }
        _ => Err((404, "not found".into())),
    }
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    image: String,
    preset: Option<String>,
    cpus: Option<u32>,
    memory: Option<String>,
    #[serde(default)]
    ports: Vec<String>,
    #[serde(default)]
    legacy_devices: bool,
    #[serde(default)]
    windows: bool,
    /// alphaOS: install it onto a new disk.
    #[serde(default)]
    install: bool,
    #[serde(default)]
    storage: Option<String>,
    disk_size: Option<String>,
    #[serde(default)]
    start: bool,
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    accel_mode: crate::engine::AccelMode,
    #[serde(default)]
    gpu: bool,
}

#[derive(Deserialize)]
struct SizeBody {
    size: String,
}

#[derive(Deserialize)]
struct SnapshotBody {
    action: String,
    name: String,
}

#[derive(Deserialize)]
struct InspectBody {
    image: String,
}

fn read_json<T: for<'de> Deserialize<'de>>(
    req: &mut Request,
) -> std::result::Result<T, (u16, String)> {
    let mut body = String::new();
    req.as_reader()
        .take(1 << 20)
        .read_to_string(&mut body)
        .context("reading body")
        .map_err(bad)?;
    serde_json::from_str(&body).map_err(|e| (400, format!("bad JSON: {e}")))
}

fn bad(e: anyhow::Error) -> (u16, String) {
    (400, format!("{e:#}"))
}

fn internal(e: anyhow::Error) -> (u16, String) {
    (500, format!("{e:#}"))
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("static header")
}
