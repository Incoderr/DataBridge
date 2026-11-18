//! Self-update from GitHub Releases.
//!
//! The Windows CI (`.github/workflows/windows.yml`) attaches to every `v*`
//! tag's release the bare exe ([`ASSET`]) and its SHA-256 ([`ASSET`] +
//! `.sha256`). The app asks the GitHub API for the latest (non-pre)release
//! ([`check`]); when its tag is newer than `CARGO_PKG_VERSION`, the UI
//! offers it. [`install`] then streams the exe next to the running one,
//! verifies the checksum and swaps the files: Windows cannot overwrite a
//! running exe but can *rename* it, so the current exe becomes
//! `databridge.exe.old` (removed by [`cleanup`] on the next start) and the
//! download takes its name. [`restart`] launches the new exe; the caller
//! quits.
//!
//! Self-install is Windows release builds only ([`can_install`]): other
//! platforms ship no binary yet, and a debug build lives in `target/`
//! where cargo owns the exe — those get a link to the release page.
//! Nothing personal is sent: the request carries only a generic
//! `DataBridge/<version>` user agent.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Write as _;
use tokio::sync::mpsc;

use crate::core::error::DbError;
use crate::core::runtime;

/// `owner/name` of the GitHub repository that publishes the releases.
pub const REPO: &str = "Incoderr/DataBridge";
/// Release asset with the bare Windows exe (and `<ASSET>.sha256`).
pub const ASSET: &str = "databridge-windows-x64.exe";

/// The running version (`Cargo.toml`).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A newer release than the running one.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    /// Version without the `v` prefix (`"0.3.0"`).
    pub version: String,
    /// The release page (notes; manual download).
    pub page_url: String,
    /// Exe + checksum download URLs, when the release carries them
    /// (releases before the self-updater only had a zip).
    pub exe_url: Option<String>,
    pub sha256_url: Option<String>,
}

impl Release {
    pub fn installable(&self) -> bool {
        can_install() && self.exe_url.is_some() && self.sha256_url.is_some()
    }
}

/// Whether this build can replace itself (see the module doc).
pub fn can_install() -> bool {
    cfg!(windows) && !cfg!(debug_assertions)
}

// ---------- version compare ----------

/// `"v1.2.3-beta.1"` → `([1, 2, 3], true)`; `None` when not a version.
fn parse_version(s: &str) -> Option<(Vec<u64>, bool)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    // `-` starts a pre-release, `+` build metadata (ignored).
    let (core, pre) = match s.find(['-', '+']) {
        Some(ix) => (&s[..ix], s.as_bytes()[ix] == b'-'),
        None => (s, false),
    };
    let nums = core
        .split('.')
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    (!nums.is_empty()).then_some((nums, pre))
}

/// `remote` is strictly newer than `current` (missing components count as
/// 0; a pre-release is older than the same version without one).
pub fn is_newer(remote: &str, current: &str) -> bool {
    let (Some((r, r_pre)), Some((c, c_pre))) = (parse_version(remote), parse_version(current))
    else {
        return false;
    };
    let len = r.len().max(c.len());
    for i in 0..len {
        let (a, b) = (r.get(i).copied().unwrap_or(0), c.get(i).copied().unwrap_or(0));
        if a != b {
            return a > b;
        }
    }
    !r_pre && c_pre
}

// ---------- check ----------

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

fn get(client: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    client
        .get(url)
        .header(reqwest::header::USER_AGENT, format!("DataBridge/{}", current_version()))
        .timeout(Duration::from_secs(30))
}

/// Pick the newer release out of the API answer (pure: tested below).
fn newer_release(rel: GhRelease, current: &str) -> Option<Release> {
    if rel.draft || rel.prerelease || !is_newer(&rel.tag_name, current) {
        return None;
    }
    let asset = |name: &str| {
        rel.assets
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(name))
            .map(|a| a.browser_download_url.clone())
    };
    Some(Release {
        version: rel.tag_name.trim_start_matches(['v', 'V']).to_string(),
        page_url: rel.html_url.clone(),
        exe_url: asset(ASSET),
        sha256_url: asset(&format!("{ASSET}.sha256")),
    })
}

async fn check_inner() -> Result<Option<Release>, DbError> {
    let client = runtime::http_client()?;
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = get(&client, &url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| DbError::with_source(format!("cannot reach GitHub: {e}"), e))?;
    let status = resp.status();
    // No published release yet.
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(DbError::new(format!(
            "GitHub answered {status}: {}",
            text.chars().take(200).collect::<String>()
        )));
    }
    let rel: GhRelease = resp
        .json()
        .await
        .map_err(|e| DbError::with_source(format!("unexpected GitHub answer: {e}"), e))?;
    Ok(newer_release(rel, current_version()))
}

/// Ask GitHub for the latest release; `Some` only when it is newer.
pub fn check() -> tokio::sync::oneshot::Receiver<Result<Option<Release>, DbError>> {
    runtime::spawn(check_inner())
}

// ---------- install ----------

/// Progress of [`install`]. The stream ends with `Done` or `Error`.
#[derive(Debug, Clone, PartialEq)]
pub enum InstallEvent {
    /// Bytes downloaded so far, total when the server sent a length.
    Progress(u64, Option<u64>),
    /// The new exe is in place; [`restart`] runs it.
    Done,
    Error(String),
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

/// The first hex token of a `sha256sum`-style file.
fn parse_checksum(text: &str) -> Option<String> {
    let token = text.split_whitespace().next()?.to_ascii_lowercase();
    (token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())).then_some(token)
}

async fn install_inner(rel: Release, tx: &mpsc::UnboundedSender<InstallEvent>) -> Result<(), DbError> {
    let (Some(exe_url), Some(sum_url)) = (rel.exe_url.as_deref(), rel.sha256_url.as_deref()) else {
        return Err(DbError::new("this release has no installable Windows build"));
    };
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize().or(Ok(p)))
        .map_err(|e| DbError::with_source("cannot locate the running exe", e))?;
    let client = runtime::http_client()?;

    let sum_text = get(&client, sum_url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| DbError::with_source(format!("cannot download the checksum: {e}"), e))?
        .text()
        .await
        .map_err(|e| DbError::with_source(format!("cannot download the checksum: {e}"), e))?;
    let expected = parse_checksum(&sum_text).ok_or_else(|| DbError::new("malformed checksum file"))?;

    let mut resp = client
        .get(exe_url)
        .header(reqwest::header::USER_AGENT, format!("DataBridge/{}", current_version()))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| DbError::with_source(format!("cannot download the update: {e}"), e))?;
    let total = resp.content_length();

    let tmp = sibling(&exe, ".download");
    // std::fs: tokio's `fs` feature is not enabled (mock-only build); the
    // chunks are small, a blocking write on a sidecar worker is fine.
    let mut file = std::fs::File::create(&tmp).map_err(|e| {
        DbError::with_source(format!("cannot write next to {}: {e}", exe.display()), e)
    })?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut reported = 0u64;
    let result: Result<(), DbError> = async {
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| DbError::with_source(format!("download interrupted: {e}"), e))?
        {
            hasher.update(&chunk);
            file.write_all(&chunk)
                .map_err(|e| DbError::with_source(format!("cannot write the update: {e}"), e))?;
            done += chunk.len() as u64;
            if done - reported >= 256 * 1024 {
                reported = done;
                let _ = tx.send(InstallEvent::Progress(done, total));
            }
        }
        file.sync_all()
            .map_err(|e| DbError::with_source(format!("cannot write the update: {e}"), e))?;
        Ok(())
    }
    .await;
    drop(file);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let _ = tx.send(InstallEvent::Progress(done, total));

    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err(DbError::new("checksum mismatch — the download is corrupt, try again"));
    }

    // Swap: running exe → .old, download → exe; undo the first rename if
    // the second fails so the app stays launchable.
    let old = sibling(&exe, ".old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        DbError::with_source(format!("cannot replace {}: {e}", exe.display()), e)
    })?;
    if let Err(e) = std::fs::rename(&tmp, &exe) {
        let _ = std::fs::rename(&old, &exe);
        let _ = std::fs::remove_file(&tmp);
        return Err(DbError::with_source(format!("cannot replace {}: {e}", exe.display()), e));
    }
    Ok(())
}

/// Download, verify and put the release's exe in place of the running one.
pub fn install(rel: Release) -> mpsc::UnboundedReceiver<InstallEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    runtime::handle().spawn(async move {
        let end = match install_inner(rel, &tx).await {
            Ok(()) => InstallEvent::Done,
            Err(e) => InstallEvent::Error(e.message().to_string()),
        };
        let _ = tx.send(end);
    });
    rx
}

/// Launch the (new) exe with the same arguments; the caller quits.
pub fn restart() -> Result<(), DbError> {
    let exe = std::env::current_exe()
        .map_err(|e| DbError::with_source("cannot locate the running exe", e))?;
    std::process::Command::new(&exe)
        .args(std::env::args_os().skip(1))
        .spawn()
        .map_err(|e| DbError::with_source(format!("cannot start {}: {e}", exe.display()), e))?;
    Ok(())
}

/// Remove what a previous update left behind (`.old`, a broken
/// `.download`). Called once at startup; failures are harmless.
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(sibling(&exe, ".old"));
        let _ = std::fs::remove_file(sibling(&exe, ".download"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("v0.3.0", "0.2.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("v1.0", "0.99.99"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
        assert!(!is_newer("0.2", "0.2.0"));
        assert!(!is_newer("v0.1.9", "0.2.0"));
        assert!(is_newer("0.2.0", "0.2.0-beta.1"));
        assert!(!is_newer("0.2.0-beta.1", "0.2.0"));
        assert!(!is_newer("nightly", "0.2.0"));
    }

    fn gh(tag: &str, assets: &[&str]) -> GhRelease {
        GhRelease {
            tag_name: tag.into(),
            html_url: format!("https://github.com/{REPO}/releases/tag/{tag}"),
            draft: false,
            prerelease: false,
            assets: assets
                .iter()
                .map(|n| GhAsset { name: (*n).into(), browser_download_url: format!("https://dl/{n}") })
                .collect(),
        }
    }

    #[test]
    fn picks_assets_of_a_newer_release() {
        let sum = format!("{ASSET}.sha256");
        let r = newer_release(gh("v0.3.0", &["x.zip", ASSET, &sum]), "0.2.0").unwrap();
        assert_eq!(r.version, "0.3.0");
        assert_eq!(r.exe_url.as_deref(), Some(format!("https://dl/{ASSET}").as_str()));
        assert_eq!(r.sha256_url.as_deref(), Some(format!("https://dl/{sum}").as_str()));
        // Old zip-only releases are offered as a page link only.
        let r = newer_release(gh("v0.3.0", &["x.zip"]), "0.2.0").unwrap();
        assert!(r.exe_url.is_none() && !r.installable());
        assert!(newer_release(gh("v0.2.0", &[ASSET]), "0.2.0").is_none());
        let mut pre = gh("v0.4.0", &[ASSET]);
        pre.prerelease = true;
        assert!(newer_release(pre, "0.2.0").is_none());
    }

    #[test]
    fn checksum_file_formats() {
        let h = "a".repeat(64);
        assert_eq!(parse_checksum(&h), Some(h.clone()));
        assert_eq!(parse_checksum(&format!("{}  {ASSET}\n", h.to_uppercase())), Some(h.clone()));
        assert_eq!(parse_checksum("deadbeef  x.exe"), None);
        assert_eq!(parse_checksum(""), None);
    }
}
