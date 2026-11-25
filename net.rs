//! Networking: HTTP with retry/backoff, GHCR pull tokens, and a streaming
//! downloader that verifies sha256 while it writes.
//!
//! Built on ureq 2.x (`rustls`). Redirects are followed manually so the
//! `Authorization` header is dropped after the first hop: ghcr.io answers
//! blob requests with a 307 to a signed CDN URL that rejects requests which
//! carry both the signature and a bearer token.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use ureq::Agent;

const UA: &str = concat!("picobrew/", env!("CARGO_PKG_VERSION"), " (+rust)");

pub fn agent(timeout: Duration) -> Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout(timeout)
        .redirects(0)
        .user_agent(UA)
        .build()
}

pub fn open_bytes(url: &str, auth: Option<&str>, accept: Option<&str>) -> Result<ureq::Response> {
    let agent = agent(Duration::from_secs(120));
    let mut req = agent.get(url);
    if let Some(a) = auth {
        req = req.set("Authorization", &format!("Bearer {a}"));
    }
    if let Some(a) = accept {
        req = req.set("Accept", a);
    }
    Ok(req.call()?)
}

/// GET a URL to completion with retries on 429/5xx/transport errors.
pub fn get_bytes_with_retry(url: &str) -> Result<Vec<u8>> {
    let mut delay = Duration::from_millis(250);
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..4 {
        match get_bytes_once(url) {
            Ok(b) => return Ok(b),
            Err(e) => {
                let retry = is_retryable(&e) && attempt < 3;
                last = Some(e);
                if !retry {
                    break;
                }
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(4));
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("request failed")))
}

fn get_bytes_once(url: &str) -> Result<Vec<u8>> {
    let agent = agent(Duration::from_secs(90));
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| format!("GET {url}"))?;
    let mut reader = resp.into_reader();
    let mut buf = Vec::with_capacity(1 << 22);
    reader.read_to_end(&mut buf).context("reading body")?;
    Ok(buf)
}

pub fn get_text(url: &str) -> Result<String> {
    let bytes = get_bytes_with_retry(url)?;
    String::from_utf8(bytes).context("response is not UTF-8")
}

fn is_retryable(e: &anyhow::Error) -> bool {
    // ureq::Error renders as `http status: 503` / transport messages; treat
    // anything that is not a 4xx as worth one more try.
    let msg = format!("{e:#}");
    if let Some(rest) = msg.split("status: ").nth(1) {
        if let Ok(code) = rest
            .chars()
            .take(3)
            .collect::<String>()
            .parse::<u16>()
        {
            return code == 429 || code >= 500;
        }
    }
    !msg.contains("status: 4")
}

// ---------------------------------------------------------------------------
// GHCR: repo parsing, anonymous pull tokens, blob resolution
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub struct GhcrRef {
    pub host: String,
    pub repo: String,
    /// A `sha256:…` digest or a tag, if the URL carried one.
    pub reference: Option<String>,
}

/// Parse `oci://ghcr.io/homebrew/core/jq:1.7.1`, a `/v2/…/blobs/sha256:…` URL
/// or a `/v2/…/manifests/<ref>` URL. Returns `None` for non-ghcr URLs.
pub fn parse_ghcr(url: &str) -> Option<GhcrRef> {
    let s = url
        .strip_prefix("oci://")
        .or_else(|| url.strip_prefix("https://"))
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let (host, rest) = s.split_once('/')?;
    if host != "ghcr.io" {
        return None;
    }
    let mut parts: Vec<&str> = rest.split('/').collect();
    if parts.first() == Some(&"v2") {
        parts.remove(0);
    }
    if let Some(pos) = parts.iter().position(|p| *p == "blobs" || *p == "manifests") {
        if pos >= 1 && pos + 1 < parts.len() {
            return Some(GhcrRef {
                host: host.to_string(),
                repo: parts[..pos].join("/"),
                reference: Some(parts[pos + 1..].join("/")),
            });
        }
    }
    // oci://host/repo:tag
    if let Some((repo, tag)) = rest.rsplit_once(':') {
        if !repo.is_empty() && !tag.is_empty() && !tag.contains('/') {
            return Some(GhcrRef {
                host: host.to_string(),
                repo: repo.to_string(),
                reference: Some(tag.to_string()),
            });
        }
    }
    Some(GhcrRef {
        host: host.to_string(),
        repo: rest.to_string(),
        reference: None,
    })
}

static TOKENS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn token_cache() -> &'static Mutex<HashMap<String, String>> {
    TOKENS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Anonymous pull token for a ghcr.io repository, cached per repository.
pub fn ghcr_token(host: &str, repo: &str) -> Result<String> {
    let key = format!("{host}/{repo}");
    if let Some(t) = token_cache().lock().unwrap().get(&key).cloned() {
        return Ok(t);
    }
    let url = format!("https://{host}/token?service={host}&scope=repository:{repo}:pull");
    let body = get_bytes_with_retry(&url).context("requesting ghcr token")?;
    #[derive(serde::Deserialize)]
    struct TokenResponse {
        token: String,
    }
    let parsed: TokenResponse =
        serde_json::from_slice(&body).context("parsing ghcr token response")?;
    token_cache()
        .lock()
        .unwrap()
        .insert(key, parsed.token.clone());
    Ok(parsed.token)
}

const OCI_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

/// For `oci://…:tag` references (no digest): resolve the tag to the first
/// layer blob URL via the manifest, so it can be downloaded like a blob.
pub fn resolve_manifest_blob(ghcr: &GhcrRef, reference: &str, token: &str) -> Result<String> {
    let url = format!(
        "https://{}/v2/{}/manifests/{}",
        ghcr.host, ghcr.repo, reference
    );
    let resp = open_bytes(&url, Some(token), Some(OCI_ACCEPT))
        .context("fetching image manifest")?;
    let mut body = String::new();
    resp.into_reader().read_to_string(&mut body)?;
    #[derive(serde::Deserialize)]
    struct Manifest {
        #[serde(default)]
        layers: Vec<Layer>,
    }
    #[derive(serde::Deserialize)]
    struct Layer {
        digest: String,
    }
    let manifest: Manifest = serde_json::from_str(&body).context("parsing image manifest")?;
    let layer = manifest
        .layers
        .first()
        .ok_or_else(|| anyhow!("manifest has no layers"))?;
    Ok(format!(
        "https://{}/v2/{}/blobs/{}",
        ghcr.host, ghcr.repo, layer.digest
    ))
}

// ---------------------------------------------------------------------------
// Downloads
// ---------------------------------------------------------------------------

/// Stream `url` into `dest`, calling `on_bytes(n)` as data arrives, then verify
/// sha256 (or receive the computed digest when `expected` is empty).
/// Returns (bytes written, hex digest).
pub fn download_verified(
    url: &str,
    dest: &std::path::Path,
    expected: &str,
    on_bytes: &(dyn Fn(u64) + Sync),
) -> Result<(u64, String)> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = dest.with_extension("part");

    let mut current = url.to_string();
    let mut auth: Option<String> = None;
    if let Some(ghcr) = parse_ghcr(url) {
        let token = ghcr_token(&ghcr.host, &ghcr.repo)?;
        current = match &ghcr.reference {
            Some(r) if r.starts_with("sha256:") => {
                format!("https://{}/v2/{}/blobs/{}", ghcr.host, ghcr.repo, r)
            }
            Some(r) => resolve_manifest_blob(&ghcr, r, &token)?,
            None => bail!("ghcr url without a digest or tag: {url}"),
        };
        auth = Some(token);
    }

    let mut hasher = crate::sha::Hasher::new();
    let mut written: u64 = 0;
    let mut hops = 0usize;
    loop {
        let resp = open_bytes(&current, auth.as_deref(), None)
            .with_context(|| format!("GET {current}"))?;
        let status = resp.status();
        if (300..400).contains(&status) {
            let loc = resp
                .header("location")
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow!("{status} redirect without location"))?;
            current = if loc.starts_with("http") {
                loc
            } else {
                format!("https://ghcr.io{loc}")
            };
            auth = None; // signed CDN URLs reject requests carrying a token
            hops += 1;
            if hops > 5 {
                bail!("too many redirects");
            }
            continue;
        }
        if status != 200 {
            bail!("GET {current} → HTTP {status}");
        }
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let mut reader = resp.into_reader();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = reader.read(&mut buf).context("reading body")?;
            if n == 0 {
                break;
            }
            let chunk = &buf[..n];
            hasher.update(chunk);
            file.write_all(chunk)?;
            written += n as u64;
            on_bytes(n as u64);
        }
        file.flush()?;
        break;
    }

    let got = hasher.hex();
    if !expected.is_empty() && got != expected.to_ascii_lowercase() {
        let _ = std::fs::remove_file(&tmp);
        bail!(
            "sha256 mismatch for {}: expected {expected}, got {got}",
            dest.display()
        );
    }
    std::fs::rename(&tmp, dest).with_context(|| format!("finalising {}", dest.display()))?;
    Ok((written, got))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_oci_tag_urls() {
        let r = parse_ghcr("oci://ghcr.io/homebrew/core/jq:1.7.1").unwrap();
        assert_eq!(r.host, "ghcr.io");
        assert_eq!(r.repo, "homebrew/core/jq");
        assert_eq!(r.reference.as_deref(), Some("1.7.1"));
    }

    #[test]
    fn parses_blob_urls() {
        let r = parse_ghcr(
            "https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:d89ea78fbc8ec79a80d52c7dcaa28e50c2bb435acd1470ee2cae4ef7646eff7e",
        )
        .unwrap();
        assert_eq!(r.repo, "homebrew/core/wget");
        assert!(r.reference.unwrap().starts_with("sha256:"));
    }

    #[test]
    fn parses_manifest_urls() {
        let r = parse_ghcr("https://ghcr.io/v2/homebrew/core/jq/manifests/1.7.1").unwrap();
        assert_eq!(r.reference.as_deref(), Some("1.7.1"));
    }

    #[test]
    fn rejects_foreign_hosts() {
        assert!(parse_ghcr("https://example.com/foo.tar.gz").is_none());
    }
}
