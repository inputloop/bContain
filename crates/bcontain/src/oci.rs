//! Run OCI / Docker container images inside a lightweight VM.
//!
//! The image's layers are flattened *in memory* (so Unix modes, owners and
//! symlinks survive even on a Windows host, whose filesystem cannot represent
//! them), then written out as a gzip'd `newc` cpio initramfs with a small
//! `/.bcontain/init` that mounts the basics, sets up networking and `exec`s
//! the image's entrypoint. QEMU boots that with a stock guest kernel.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Reading images

/// Somewhere blobs can be read from by relative path.
enum Source {
    Dir(PathBuf),
    /// Uncompressed tar: entry name -> (offset, size).
    Tar(PathBuf, HashMap<String, (u64, u64)>),
}

impl Source {
    fn open(path: &Path) -> Result<Source> {
        if path.is_dir() {
            return Ok(Source::Dir(path.to_path_buf()));
        }
        let mut index = HashMap::new();
        let mut ar = tar::Archive::new(File::open(path)?);
        for e in ar.entries()? {
            let e = e?;
            let name = e
                .path()?
                .to_string_lossy()
                .trim_start_matches("./")
                .to_string();
            index.insert(name, (e.raw_file_position(), e.size()));
        }
        Ok(Source::Tar(path.to_path_buf(), index))
    }

    fn has(&self, name: &str) -> bool {
        match self {
            Source::Dir(d) => d.join(name).is_file(),
            Source::Tar(_, idx) => idx.contains_key(name),
        }
    }

    fn reader(&self, name: &str) -> Result<Box<dyn Read>> {
        match self {
            Source::Dir(d) => Ok(Box::new(
                File::open(d.join(name)).with_context(|| format!("missing blob {name}"))?,
            )),
            Source::Tar(p, idx) => {
                let &(off, size) = idx
                    .get(name)
                    .ok_or_else(|| anyhow!("missing {name} in archive"))?;
                let mut f = File::open(p)?;
                f.seek(SeekFrom::Start(off))?;
                Ok(Box::new(f.take(size)))
            }
        }
    }

    fn read(&self, name: &str) -> Result<Vec<u8>> {
        let mut v = Vec::new();
        self.reader(name)?.read_to_end(&mut v)?;
        Ok(v)
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerConfig {
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<Vec<String>>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub exposed_ports: Option<BTreeMap<String, serde_json::Value>>,
}

#[derive(Deserialize)]
struct ImageConfig {
    #[serde(default)]
    config: ContainerConfig,
    #[serde(default)]
    architecture: Option<String>,
}

/// A resolved image: its run config and its layer blobs, bottom first.
pub struct Image {
    source: Source,
    pub config: ContainerConfig,
    pub architecture: String,
    layers: Vec<String>,
    pub tags: Vec<String>,
}

fn blob_path(digest: &str) -> Result<String> {
    let (alg, hex) = digest
        .split_once(':')
        .ok_or_else(|| anyhow!("bad digest {digest}"))?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("bad digest {digest}");
    }
    Ok(format!("blobs/{alg}/{hex}"))
}

impl Image {
    /// Open a `docker save` tarball, an OCI layout tarball, or an OCI layout directory.
    pub fn open(path: &Path) -> Result<Image> {
        let source = Source::open(path)?;
        if source.has("manifest.json") {
            // docker save (also written by Docker 25+ alongside the OCI layout)
            #[derive(Deserialize)]
            #[serde(rename_all = "PascalCase")]
            struct Entry {
                config: String,
                #[serde(default)]
                repo_tags: Option<Vec<String>>,
                layers: Vec<String>,
            }
            let m: Vec<Entry> = serde_json::from_slice(&source.read("manifest.json")?)?;
            let e = m
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("empty manifest.json"))?;
            let cfg: ImageConfig = serde_json::from_slice(&source.read(&e.config)?)?;
            return Ok(Image {
                config: cfg.config,
                architecture: cfg.architecture.unwrap_or_else(|| "amd64".into()),
                layers: e.layers,
                tags: e.repo_tags.unwrap_or_default(),
                source,
            });
        }
        if source.has("index.json") {
            let index: serde_json::Value = serde_json::from_slice(&source.read("index.json")?)?;
            let manifest = resolve_manifest(&source, &index)?;
            let cfg_digest = manifest["config"]["digest"]
                .as_str()
                .ok_or_else(|| anyhow!("manifest has no config"))?;
            let cfg: ImageConfig = serde_json::from_slice(&source.read(&blob_path(cfg_digest)?)?)?;
            let layers = manifest["layers"]
                .as_array()
                .ok_or_else(|| anyhow!("manifest has no layers"))?
                .iter()
                .map(|l| blob_path(l["digest"].as_str().unwrap_or_default()))
                .collect::<Result<Vec<_>>>()?;
            let tags = index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"]
                .as_str()
                .map(|s| vec![s.to_string()])
                .unwrap_or_default();
            return Ok(Image {
                config: cfg.config,
                architecture: cfg.architecture.unwrap_or_else(|| "amd64".into()),
                layers,
                tags,
                source,
            });
        }
        bail!(
            "{} is neither a `docker save` archive nor an OCI image layout",
            path.display()
        )
    }

    /// Flatten all layers into one filesystem tree.
    pub fn flatten(&self) -> Result<Tree> {
        let mut tree = Tree::default();
        for layer in &self.layers {
            let mut r = self.source.reader(layer)?;
            let mut magic = [0u8; 4];
            let n = read_full(&mut r, &mut magic)?;
            let chained = io::Cursor::new(magic[..n].to_vec()).chain(r);
            if magic[..2] == [0x1f, 0x8b] {
                tree.apply_layer(GzDecoder::new(chained))?;
            } else if magic == [0x28, 0xb5, 0x2f, 0xfd] {
                bail!("layer {layer} is zstd-compressed, which bContain does not read yet");
            } else {
                tree.apply_layer(chained)?;
            }
        }
        Ok(tree)
    }

    /// The command line the container runs: Entrypoint + (override or Cmd).
    pub fn argv(&self, override_cmd: &[String]) -> Vec<String> {
        let mut argv = self.config.entrypoint.clone().unwrap_or_default();
        if override_cmd.is_empty() {
            argv.extend(self.config.cmd.clone().unwrap_or_default());
        } else {
            argv.extend(override_cmd.iter().cloned());
        }
        argv
    }
}

fn resolve_manifest(source: &Source, index: &serde_json::Value) -> Result<serde_json::Value> {
    let descs = index["manifests"]
        .as_array()
        .ok_or_else(|| anyhow!("index has no manifests"))?;
    let pick = descs
        .iter()
        .find(|d| {
            let p = &d["platform"];
            p.is_null() || (p["os"] == "linux" && p["architecture"] == "amd64")
        })
        .ok_or_else(|| anyhow!("image has no linux/amd64 variant"))?;
    let digest = pick["digest"]
        .as_str()
        .ok_or_else(|| anyhow!("descriptor without digest"))?;
    let m: serde_json::Value = serde_json::from_slice(&source.read(&blob_path(digest)?)?)?;
    if m.get("manifests").is_some() {
        resolve_manifest(source, &m) // nested index
    } else {
        Ok(m)
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------------------
// The flattened filesystem

#[derive(Debug, Clone)]
pub enum Kind {
    Dir,
    File(Vec<u8>),
    Symlink(String),
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: Kind,
    /// Permission bits only (no file-type bits).
    pub perm: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u32,
}

/// Paths are stored relative to `/` with no leading slash.
#[derive(Default)]
pub struct Tree {
    pub nodes: BTreeMap<String, Node>,
}

fn normalize(p: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            c => parts.push(c),
        }
    }
    Some(parts.join("/"))
}

impl Tree {
    fn remove_subtree(&mut self, path: &str) {
        self.nodes.remove(path);
        self.remove_children(path);
    }

    fn remove_children(&mut self, dir: &str) {
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let doomed: Vec<String> = self
            .nodes
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .map(|(k, _)| k.clone())
            .collect();
        for k in doomed {
            self.nodes.remove(&k);
        }
    }

    pub fn insert(&mut self, path: &str, node: Node) {
        // A non-directory replacing a directory hides everything under it.
        if !matches!(node.kind, Kind::Dir) {
            self.remove_children(path);
        }
        self.nodes.insert(path.to_string(), node);
    }

    /// Apply one layer tarball, honouring OCI whiteouts.
    pub fn apply_layer(&mut self, r: impl Read) -> Result<()> {
        let mut ar = tar::Archive::new(r);
        let mut opaque = Vec::new();
        let mut whiteouts = Vec::new();
        let mut adds: Vec<(String, Node)> = Vec::new();
        let mut hardlinks: Vec<(String, String, Node)> = Vec::new();

        for e in ar.entries()? {
            let mut e = e?;
            let raw = e.path()?.to_string_lossy().to_string();
            let Some(path) = normalize(&raw) else {
                continue;
            };
            if path.is_empty() {
                continue;
            }
            let (dir, base) = match path.rsplit_once('/') {
                Some((d, b)) => (d.to_string(), b.to_string()),
                None => (String::new(), path.clone()),
            };
            if base == ".wh..wh..opq" {
                opaque.push(dir);
                continue;
            }
            if let Some(hidden) = base.strip_prefix(".wh.") {
                whiteouts.push(if dir.is_empty() {
                    hidden.to_string()
                } else {
                    format!("{dir}/{hidden}")
                });
                continue;
            }
            let h = e.header();
            let mut node = Node {
                kind: Kind::Dir,
                perm: h.mode().unwrap_or(0o755) & 0o7777,
                uid: h.uid().unwrap_or(0) as u32,
                gid: h.gid().unwrap_or(0) as u32,
                mtime: h.mtime().unwrap_or(0) as u32,
            };
            match h.entry_type() {
                tar::EntryType::Directory => adds.push((path, node)),
                tar::EntryType::Regular | tar::EntryType::Continuous => {
                    let mut data = Vec::with_capacity(e.size() as usize);
                    e.read_to_end(&mut data)?;
                    node.kind = Kind::File(data);
                    adds.push((path, node));
                }
                tar::EntryType::Symlink => {
                    let target = e
                        .link_name()?
                        .map(|t| t.to_string_lossy().to_string())
                        .unwrap_or_default();
                    node.kind = Kind::Symlink(target);
                    adds.push((path, node));
                }
                tar::EntryType::Link => {
                    let target = e
                        .link_name()?
                        .map(|t| t.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if let Some(t) = normalize(&target) {
                        hardlinks.push((path, t, node));
                    }
                }
                // Device nodes, FIFOs: /dev is a devtmpfs in the guest.
                _ => {}
            }
        }

        for d in opaque {
            self.remove_children(&d);
        }
        for w in whiteouts {
            self.remove_subtree(&w);
        }
        for (p, n) in adds {
            self.insert(&p, n);
        }
        // Hard links become copies; cpio could share inodes, but copies are
        // simpler and the initramfs lives in RAM either way.
        for (p, target, mut n) in hardlinks {
            if let Some(Node {
                kind: Kind::File(data),
                ..
            }) = self.nodes.get(&target)
            {
                n.kind = Kind::File(data.clone());
                self.insert(&p, n);
            }
        }
        Ok(())
    }

    /// Make sure every node's parent directories exist.
    fn fill_parents(&mut self) {
        let mut missing = Vec::new();
        for path in self.nodes.keys() {
            let mut p = path.as_str();
            while let Some((parent, _)) = p.rsplit_once('/') {
                if !self.nodes.contains_key(parent) {
                    missing.push(parent.to_string());
                }
                p = parent;
            }
        }
        for m in missing {
            self.nodes.entry(m).or_insert(Node {
                kind: Kind::Dir,
                perm: 0o755,
                uid: 0,
                gid: 0,
                mtime: 0,
            });
        }
    }

    pub fn total_bytes(&self) -> u64 {
        self.nodes
            .values()
            .map(|n| match &n.kind {
                Kind::File(d) => d.len() as u64,
                _ => 0,
            })
            .sum()
    }

    /// Write the tree as a gzip-compressed newc cpio archive.
    pub fn write_initramfs(&mut self, out: &Path) -> Result<()> {
        self.fill_parents();
        for d in ["proc", "sys", "dev", "tmp", "run", "etc"] {
            self.nodes.entry(d.into()).or_insert(Node {
                kind: Kind::Dir,
                perm: if d == "tmp" { 0o1777 } else { 0o755 },
                uid: 0,
                gid: 0,
                mtime: 0,
            });
        }
        let f = BufWriter::new(
            File::create(out).with_context(|| format!("creating {}", out.display()))?,
        );
        let mut w = CpioWriter {
            out: GzEncoder::new(f, flate2::Compression::fast()),
            ino: 1,
        };
        for (path, node) in &self.nodes {
            w.entry(path, node)?;
        }
        w.trailer()?;
        w.out.finish()?.flush()?;
        Ok(())
    }
}

struct CpioWriter<W: Write> {
    out: W,
    ino: u32,
}

impl<W: Write> CpioWriter<W> {
    fn header(
        &mut self,
        name: &str,
        mode: u32,
        n: &Node,
        size: usize,
        nlink: u32,
    ) -> io::Result<()> {
        let namesize = name.len() + 1;
        write!(
            self.out,
            "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
            self.ino, mode, n.uid, n.gid, nlink, n.mtime, size, 0, 0, 0, 0, namesize, 0
        )?;
        self.ino += 1;
        self.out.write_all(name.as_bytes())?;
        self.out.write_all(&[0])?;
        self.pad(110 + namesize)
    }

    fn pad(&mut self, len: usize) -> io::Result<()> {
        let rem = (4 - len % 4) % 4;
        self.out.write_all(&[0u8; 3][..rem])
    }

    fn entry(&mut self, path: &str, n: &Node) -> io::Result<()> {
        match &n.kind {
            Kind::Dir => self.header(path, 0o040000 | n.perm, n, 0, 2),
            Kind::File(data) => {
                self.header(path, 0o100000 | n.perm, n, data.len(), 1)?;
                self.out.write_all(data)?;
                self.pad(data.len())
            }
            Kind::Symlink(t) => {
                self.header(path, 0o120000 | 0o777, n, t.len(), 1)?;
                self.out.write_all(t.as_bytes())?;
                self.pad(t.len())
            }
        }
    }

    fn trailer(&mut self) -> io::Result<()> {
        let n = Node {
            kind: Kind::File(Vec::new()),
            perm: 0,
            uid: 0,
            gid: 0,
            mtime: 0,
        };
        self.ino = 0;
        self.header("TRAILER!!!", 0, &n, 0, 1)
    }
}

// ---------------------------------------------------------------------------
// The guest-side init

pub struct RunOptions {
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub workdir: String,
    pub hostname: String,
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The `/.bcontain/init` script. It needs a POSIX `/bin/sh` in the image
/// (true for alpine, debian, ubuntu, busybox-based images, ...).
pub fn init_script(o: &RunOptions) -> String {
    let mut s = String::from(
        r#"#!/bin/sh
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sysfs /sys 2>/dev/null
mount -t devtmpfs devtmpfs /dev 2>/dev/null
mkdir -p /dev/pts /dev/shm
mount -t devpts devpts /dev/pts 2>/dev/null
mount -t tmpfs tmpfs /dev/shm 2>/dev/null
ln -sf /proc/self/fd /dev/fd 2>/dev/null
"#,
    );
    s += &format!("hostname {} 2>/dev/null\n", sh_quote(&o.hostname));
    s += &format!(
        "echo {} > /etc/hostname 2>/dev/null\n",
        sh_quote(&o.hostname)
    );
    s += r#"[ -s /etc/resolv.conf ] || echo 'nameserver 10.0.2.3' > /etc/resolv.conf
if command -v ip >/dev/null 2>&1; then
  ip link set lo up; ip link set eth0 up 2>/dev/null
  ip addr add 10.0.2.15/24 dev eth0 2>/dev/null; ip route add default via 10.0.2.2 2>/dev/null
elif command -v ifconfig >/dev/null 2>&1; then
  ifconfig lo up; ifconfig eth0 10.0.2.15 netmask 255.255.255.0 up 2>/dev/null
  route add default gw 10.0.2.2 2>/dev/null
fi
"#;
    for e in &o.env {
        if e.split_once('=').is_some_and(|(k, _)| {
            !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }) {
            s += &format!("export {}\n", sh_quote_assignment(e));
        }
    }
    s += &format!("cd {} 2>/dev/null || cd /\n", sh_quote(&o.workdir));
    s += "echo '[bcontain] container started'\n";
    let argv: Vec<String> = o.argv.iter().map(|a| sh_quote(a)).collect();
    s += &argv.join(" ");
    s += r#"
rc=$?
echo "[bcontain] container exited with code $rc"
sync
poweroff -f 2>/dev/null
echo o > /proc/sysrq-trigger 2>/dev/null
exit $rc
"#;
    s
}

fn sh_quote_assignment(e: &str) -> String {
    let (k, v) = e.split_once('=').unwrap_or((e, ""));
    format!("{k}={}", sh_quote(v))
}

/// Build the initramfs for `image` with `opts`, writing it to `out`.
pub fn build_initramfs(image: &Image, opts: &RunOptions, out: &Path) -> Result<u64> {
    if image.architecture != "amd64" {
        bail!(
            "image is {}; bContain runs linux/amd64 images",
            image.architecture
        );
    }
    let mut tree = image.flatten()?;
    let has_sh = match tree.nodes.get("bin/sh") {
        Some(_) => true,
        None => tree.nodes.contains_key("usr/bin/sh"),
    };
    if !has_sh {
        bail!(
            "image has no /bin/sh; bContain's guest init is a shell script, so \
             distroless/scratch images are not supported yet"
        );
    }
    tree.insert(
        ".bcontain",
        Node {
            kind: Kind::Dir,
            perm: 0o755,
            uid: 0,
            gid: 0,
            mtime: 0,
        },
    );
    tree.insert(
        ".bcontain/init",
        Node {
            kind: Kind::File(init_script(opts).into_bytes()),
            perm: 0o755,
            uid: 0,
            gid: 0,
            mtime: 0,
        },
    );
    let bytes = tree.total_bytes();
    tree.write_initramfs(out)?;
    Ok(bytes)
}

/// Kernel command line for a container micro-VM.
pub fn kernel_cmdline() -> String {
    "console=ttyS0 rdinit=/.bcontain/init panic=-1 quiet loglevel=3 \
     ip=10.0.2.15::10.0.2.2:255.255.255.0::eth0:off"
        .to_string()
}

/// Locate the guest kernel: `--kernel`, `$BCONTAIN_KERNEL`, `kernel/vmlinuz`
/// next to the executable, or `<data>/kernel/vmlinuz`.
pub fn find_kernel(explicit: Option<&Path>) -> Result<PathBuf> {
    let mut c: Vec<PathBuf> = Vec::new();
    if let Some(p) = explicit {
        c.push(p.to_path_buf());
    }
    if let Some(p) = std::env::var_os("BCONTAIN_KERNEL") {
        c.push(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            c.push(d.join("kernel").join("vmlinuz"));
        }
    }
    c.push(crate::store::kernel_dir().join("vmlinuz"));
    c.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        anyhow!(
            "no guest kernel found. Run `bcontain kernel fetch` to download one, or pass \
             --kernel / set BCONTAIN_KERNEL to a bzImage with virtio built in"
        )
    })
}

/// Write `data` into an OCI image layout at `dir` as a content-addressed blob.
pub fn write_blob(dir: &Path, data: &[u8]) -> Result<String> {
    let hex = format!("{:x}", Sha256::digest(data));
    let p = dir.join("blobs").join("sha256");
    fs::create_dir_all(&p)?;
    fs::write(p.join(&hex), data)?;
    Ok(format!("sha256:{hex}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, data) in entries {
            let mut h = tar::Header::new_gnu();
            match data {
                Some(d) => {
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_size(d.len() as u64);
                    h.set_mode(0o644);
                    h.set_cksum();
                    b.append_data(&mut h, path, *d).unwrap();
                }
                None => {
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_size(0);
                    h.set_mode(0o755);
                    h.set_cksum();
                    b.append_data(&mut h, path, io::empty()).unwrap();
                }
            }
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn whiteouts_and_opaque() {
        let mut t = Tree::default();
        t.apply_layer(
            &layer(&[
                ("etc/", None),
                ("etc/a", Some(b"a")),
                ("etc/b", Some(b"b")),
                ("var/", None),
                ("var/x", Some(b"x")),
            ])[..],
        )
        .unwrap();
        t.apply_layer(
            &layer(&[
                ("etc/.wh.a", Some(b"")),
                ("var/.wh..wh..opq", Some(b"")),
                ("var/y", Some(b"y")),
            ])[..],
        )
        .unwrap();
        assert!(!t.nodes.contains_key("etc/a"));
        assert!(t.nodes.contains_key("etc/b"));
        assert!(!t.nodes.contains_key("var/x"));
        assert!(t.nodes.contains_key("var/y"));
    }

    #[test]
    fn cpio_is_well_formed() {
        let mut t = Tree::default();
        t.apply_layer(&layer(&[("bin/sh", Some(b"#!")), ("a/b/c", Some(b"hello"))])[..])
            .unwrap();
        let out = std::env::temp_dir().join(format!("bcontain-cpio-{}.gz", std::process::id()));
        t.write_initramfs(&out).unwrap();
        let mut raw = Vec::new();
        GzDecoder::new(File::open(&out).unwrap())
            .read_to_end(&mut raw)
            .unwrap();
        assert!(raw.starts_with(b"070701"));
        assert!(raw.windows(10).any(|w| w == b"TRAILER!!!"));
        assert_eq!(raw.len() % 4, 0);
        // parent dirs were synthesized
        assert!(t.nodes.contains_key("a") && t.nodes.contains_key("a/b"));
    }

    #[test]
    fn init_quotes_args() {
        let s = init_script(&RunOptions {
            argv: vec!["echo".into(), "it's".into()],
            env: vec!["A=1 2".into(), "bad key=x".into()],
            workdir: "/".into(),
            hostname: "h".into(),
        });
        assert!(s.contains(r"'echo' 'it'\''s'"));
        assert!(s.contains("export A='1 2'"));
        assert!(!s.contains("bad key"));
    }
}
