// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! Minimal OCI distribution client: pull public linux/amd64 images from
//! Docker Hub or any v2 registry into a local OCI image layout, so no Docker
//! install is needed.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.docker.distribution.manifest.v2+json";

#[derive(Debug, PartialEq, Eq)]
pub struct Reference {
    pub registry: String,
    pub repository: String,
    /// Tag or `sha256:` digest.
    pub reference: String,
}

impl Reference {
    pub fn parse(s: &str) -> Result<Reference> {
        let (name, reference) = if let Some((n, d)) = s.split_once('@') {
            (n, d.to_string())
        } else {
            match s.rsplit_once(':') {
                Some((n, t)) if !t.contains('/') => (n, t.to_string()),
                _ => (s, "latest".to_string()),
            }
        };
        let (registry, repo) = match name.split_once('/') {
            Some((first, rest))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_string(), rest.to_string())
            }
            _ => ("registry-1.docker.io".to_string(), name.to_string()),
        };
        let repository = if registry == "registry-1.docker.io" && !repo.contains('/') {
            format!("library/{repo}")
        } else {
            repo
        };
        if repository.is_empty() || reference.is_empty() {
            bail!("invalid image reference `{s}`");
        }
        Ok(Reference {
            registry,
            repository,
            reference,
        })
    }

    pub fn display(&self) -> String {
        let sep = if self.reference.starts_with("sha256:") {
            '@'
        } else {
            ':'
        };
        format!(
            "{}/{}{sep}{}",
            self.registry, self.repository, self.reference
        )
    }
}

pub struct Client {
    agent: ureq::Agent,
    token: Option<String>,
}

impl Client {
    pub fn new() -> Client {
        let agent = ureq::AgentBuilder::new()
            .try_proxy_from_env(true)
            .user_agent(concat!("bcontain/", env!("CARGO_PKG_VERSION")))
            .build();
        Client { agent, token: None }
    }

    fn get(&mut self, r: &Reference, path: &str, accept: Option<&str>) -> Result<ureq::Response> {
        let url = format!("https://{}/v2/{}/{path}", r.registry, r.repository);
        for attempt in 0..2 {
            let mut req = self.agent.get(&url);
            if let Some(a) = accept {
                req = req.set("Accept", a);
            }
            if let Some(t) = &self.token {
                req = req.set("Authorization", &format!("Bearer {t}"));
            }
            match req.call() {
                Ok(resp) => return Ok(resp),
                Err(ureq::Error::Status(401, resp)) if attempt == 0 => {
                    let challenge = resp
                        .header("www-authenticate")
                        .unwrap_or_default()
                        .to_string();
                    self.token = Some(self.fetch_token(&challenge)?);
                }
                Err(ureq::Error::Status(code, resp)) => {
                    let body = resp.into_string().unwrap_or_default();
                    bail!(
                        "GET {url}: HTTP {code}: {}",
                        body.chars().take(300).collect::<String>()
                    );
                }
                Err(e) => return Err(anyhow!(e).context(format!("GET {url}"))),
            }
        }
        bail!("GET {url}: still unauthorized after fetching a token (private image?)")
    }

    /// Anonymous bearer-token flow (`WWW-Authenticate: Bearer realm=...`).
    fn fetch_token(&self, challenge: &str) -> Result<String> {
        let rest = challenge
            .strip_prefix("Bearer ")
            .ok_or_else(|| anyhow!("unsupported auth challenge `{challenge}`"))?;
        let mut realm = None;
        let mut query = Vec::new();
        for part in split_params(rest) {
            if let Some((k, v)) = part.split_once('=') {
                let v = v.trim_matches('"').to_string();
                if k == "realm" {
                    realm = Some(v);
                } else {
                    query.push((k.to_string(), v));
                }
            }
        }
        let realm = realm.ok_or_else(|| anyhow!("auth challenge without realm"))?;
        let mut req = self.agent.get(&realm);
        for (k, v) in &query {
            req = req.query(k, v);
        }
        let v: Value = req.call().context("fetching registry token")?.into_json()?;
        v["token"]
            .as_str()
            .or_else(|| v["access_token"].as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("token response without token"))
    }

    fn manifest(&mut self, r: &Reference, reference: &str) -> Result<(Vec<u8>, String)> {
        let resp = self.get(r, &format!("manifests/{reference}"), Some(ACCEPT))?;
        let ct = resp.content_type().to_string();
        let mut body = Vec::new();
        resp.into_reader().take(16 << 20).read_to_end(&mut body)?;
        Ok((body, ct))
    }

    /// Pull `r` into an OCI image layout at `dir`. Calls `progress(done, total)`
    /// after each blob.
    pub fn pull(
        &mut self,
        r: &Reference,
        dir: &Path,
        mut progress: impl FnMut(&str, usize, usize),
    ) -> Result<()> {
        let (mut body, mut ct) = self.manifest(r, &r.reference)?;
        let mut v: Value = serde_json::from_slice(&body)?;
        if v.get("manifests").is_some() {
            let pick = v["manifests"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|m| {
                    m["platform"]["os"] == "linux" && m["platform"]["architecture"] == "amd64"
                })
                .ok_or_else(|| anyhow!("{} has no linux/amd64 image", r.display()))?;
            let digest = pick["digest"].as_str().unwrap_or_default().to_string();
            (body, ct) = self.manifest(r, &digest)?;
            v = serde_json::from_slice(&body)?;
        }
        if v["schemaVersion"] != 2 {
            bail!("unsupported manifest schema for {}", r.display());
        }

        fs::create_dir_all(dir)?;
        let mut blobs: Vec<String> = vec![v["config"]["digest"]
            .as_str()
            .unwrap_or_default()
            .to_string()];
        for l in v["layers"]
            .as_array()
            .ok_or_else(|| anyhow!("manifest has no layers"))?
        {
            blobs.push(l["digest"].as_str().unwrap_or_default().to_string());
        }
        let total = blobs.len();
        for (i, digest) in blobs.iter().enumerate() {
            self.blob(r, digest, dir)?;
            progress(digest, i + 1, total);
        }

        let manifest_digest = crate::oci::write_blob(dir, &body)?;
        let media = if ct.is_empty() {
            "application/vnd.oci.image.manifest.v1+json".into()
        } else {
            ct
        };
        let index = json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": media,
                "digest": manifest_digest,
                "size": body.len(),
                "annotations": { "org.opencontainers.image.ref.name": r.display() },
                "platform": { "os": "linux", "architecture": "amd64" }
            }]
        });
        fs::write(dir.join("index.json"), serde_json::to_vec_pretty(&index)?)?;
        fs::write(dir.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#)?;
        Ok(())
    }

    fn blob(&mut self, r: &Reference, digest: &str, dir: &Path) -> Result<()> {
        let hex = digest
            .strip_prefix("sha256:")
            .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
            .ok_or_else(|| anyhow!("unsupported digest `{digest}`"))?;
        let target = dir.join("blobs").join("sha256").join(hex);
        if target.is_file() {
            return Ok(());
        }
        fs::create_dir_all(target.parent().unwrap())?;
        let tmp = target.with_extension("partial");
        let mut reader = self.get(r, &format!("blobs/{digest}"), None)?.into_reader();
        let mut out = File::create(&tmp)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
        }
        drop(out);
        let got = format!("{:x}", hasher.finalize());
        if got != hex {
            let _ = fs::remove_file(&tmp);
            bail!("blob {digest} failed verification (got sha256:{got})");
        }
        fs::rename(&tmp, &target)?;
        Ok(())
    }
}

/// Split `a="x,y",b=z` on commas outside quotes.
fn split_params(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_refs() {
        let r = Reference::parse("alpine").unwrap();
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "library/alpine");
        assert_eq!(r.reference, "latest");

        let r = Reference::parse("ghcr.io/foo/bar:1.2").unwrap();
        assert_eq!(
            (
                r.registry.as_str(),
                r.repository.as_str(),
                r.reference.as_str()
            ),
            ("ghcr.io", "foo/bar", "1.2")
        );

        let r = Reference::parse("localhost:5000/x").unwrap();
        assert_eq!(
            (r.registry.as_str(), r.reference.as_str()),
            ("localhost:5000", "latest")
        );

        let r = Reference::parse("user/app@sha256:abc").unwrap();
        assert_eq!(
            (r.repository.as_str(), r.reference.as_str()),
            ("user/app", "sha256:abc")
        );
    }

    #[test]
    fn splits_challenge() {
        let p =
            split_params(r#"realm="https://a/token",service="reg",scope="repository:x:pull,push""#);
        assert_eq!(p.len(), 3);
        assert_eq!(p[2], r#"scope="repository:x:pull,push""#);
    }
}
