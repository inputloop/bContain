//! Local web dashboard: `bcontain ui` serves it on 127.0.0.1 only.
//!
//! State-changing requests must carry `X-Bcontain: 1`. Browsers will not send
//! a custom header cross-origin without a CORS preflight (which this server
//! never approves), so other websites cannot drive your VMs.

use std::io::Read;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::host::Host;
use crate::ops;
use crate::preset::Preset;
use crate::store;

const INDEX: &str = include_str!("ui/index.html");

pub fn serve(port: u16, open: bool) -> Result<()> {
    let addr = format!("127.0.0.1:{port}");
    let server = Server::http(&addr).map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))?;
    let url = format!("http://{addr}/");
    println!("bContain dashboard on {url}  (Ctrl+C to stop)");
    if open {
        open_browser(&url);
    }
    let host = Host::detect();
    for req in server.incoming_requests() {
        let host = host.clone();
        std::thread::spawn(move || handle(req, &host));
    }
    Ok(())
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    let r = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(not(windows))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    if r.is_err() {
        eprintln!("could not open a browser; visit {url}");
    }
}

fn handle(mut req: Request, host: &Host) {
    let method = req.method().clone();
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
            })))
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
                ..Default::default()
            };
            let mut lines = Vec::new();
            let rec = ops::create(&opts, host, true, &mut |l| lines.push(l)).map_err(bad)?;
            Ok(Reply::Json(json!({ "name": rec.spec.name, "log": lines })))
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
