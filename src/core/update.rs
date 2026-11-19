//! Updates from GitHub Releases, the way installed Windows programs do it.
//!
//! DataBridge ships as an Inno Setup installer (`installer/databridge.iss`,
//! per-user into `%LOCALAPPDATA%\Programs\DataBridge`, Start menu entry,
//! uninstaller in "Installed apps") plus a portable zip. The Windows CI
//! (`.github/workflows/windows.yml`) attaches to every `v*` release the
//! installer ([`SETUP_ASSET`]) and its SHA-256 (`<SETUP_ASSET>.sha256`).
//!
//! The app asks the GitHub API for the latest (non-pre)release ([`check`]);
//! when its tag is newer than `CARGO_PKG_VERSION`, the UI offers it.
//! [`download`] streams the installer into the temp dir and verifies the
//! checksum; [`run_installer`] starts it and the caller quits:
//! - an installed copy ([`InstallKind::Installed`]: `unins000.exe` next to
//!   the exe) runs it silently with `/update=1` — the setup waits for this
//!   process's instance mutex ([`hold_instance_mutex`]) to disappear,
//!   installs over the old version and starts the new one;
//! - a portable copy runs it interactively: updating means installing.
//!
//! Debug builds (cargo owns the exe in `target/`) and other platforms
//! ([`InstallKind::Unsupported`]) get a link to the release page.
//! Nothing personal is sent: the requests carry only a generic
//! `DataBridge/<version>` user agent.
//!
//! 0.3 / 0.4 updated by swapping the exe in place; CI still publishes that
//! bare exe ([`LEGACY_ASSET`]) so those versions can reach this one, and
//! [`cleanup`] removes the `.old` file such a swap leaves.

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
/// Release asset with the Windows installer (and `<SETUP_ASSET>.sha256`).
pub const SETUP_ASSET: &str = "DataBridge-Setup-x64.exe";
/// The bare exe the 0.3 / 0.4 updater swaps in (see the module doc);
/// CI publishes it, this version never downloads it.
#[allow(dead_code)]
pub const LEGACY_ASSET: &str = "databridge-windows-x64.exe";
/// Named mutex held while the app runs; the installer waits for it.
pub const INSTANCE_MUTEX: &str = "DataBridge.Instance";

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
    /// Installer + checksum download URLs, when the release carries them
    /// (releases before 0.5 had no installer).
    pub setup_url: Option<String>,
    pub sha256_url: Option<String>,
}

impl Release {
    pub fn installable(&self) -> bool {
        install_kind() != InstallKind::Unsupported && self.setup_url.is_some() && self.sha256_url.is_some()
    }
}

/// How this copy of the app was deployed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    /// Installed by the setup: updates run the setup silently.
    Installed,
    /// The portable zip: an update installs DataBridge (interactive setup).
    Portable,
    /// A debug build or a platform without an installer: release page only.
    Unsupported,
}

/// An exe directory with Inno Setup's uninstaller is an installed copy.
fn kind_of_dir(dir: &Path) -> InstallKind {
    if dir.join("unins000.exe").is_file() { InstallKind::Installed } else { InstallKind::Portable }
}

pub fn install_kind() -> InstallKind {
    if !cfg!(windows) || cfg!(debug_assertions) {
        return InstallKind::Unsupported;
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(kind_of_dir))
        .unwrap_or(InstallKind::Portable)
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
        setup_url: asset(SETUP_ASSET),
        sha256_url: asset(&format!("{SETUP_ASSET}.sha256")),
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

// ---------- download + install ----------

/// Progress of [`download`]. The stream ends with `Done` or `Error`.
#[derive(Debug, Clone, PartialEq)]
pub enum InstallEvent {
    /// Bytes downloaded so far, total when the server sent a length.
    Progress(u64, Option<u64>),
    /// The verified installer; [`run_installer`] starts it.
    Done(PathBuf),
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

/// Where installers are downloaded (one per version; older ones are
/// removed by [`cleanup`]).
fn download_dir() -> PathBuf {
    std::env::temp_dir().join("DataBridge-update")
}

async fn download_inner(rel: Release, tx: &mpsc::UnboundedSender<InstallEvent>) -> Result<PathBuf, DbError> {
    let (Some(setup_url), Some(sum_url)) = (rel.setup_url.as_deref(), rel.sha256_url.as_deref()) else {
        return Err(DbError::new("this release has no Windows installer"));
    };
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

    // No overall timeout here: the installer may take a while on a slow line.
    let mut resp = client
        .get(setup_url)
        .header(reqwest::header::USER_AGENT, format!("DataBridge/{}", current_version()))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| DbError::with_source(format!("cannot download the update: {e}"), e))?;
    let total = resp.content_length();

    let dir = download_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| DbError::with_source(format!("cannot create {}: {e}", dir.display()), e))?;
    let target = dir.join(format!("DataBridge-Setup-{}.exe", rel.version));
    let tmp = sibling(&target, ".part");
    // std::fs: tokio's `fs` feature is not enabled (mock-only build); the
    // chunks are small, a blocking write on a sidecar worker is fine.
    let mut file = std::fs::File::create(&tmp)
        .map_err(|e| DbError::with_source(format!("cannot write {}: {e}", tmp.display()), e))?;
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
    let _ = std::fs::remove_file(&target);
    std::fs::rename(&tmp, &target)
        .map_err(|e| DbError::with_source(format!("cannot write {}: {e}", target.display()), e))?;
    Ok(target)
}

/// Download and verify the release's installer.
pub fn download(rel: Release) -> mpsc::UnboundedReceiver<InstallEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    runtime::handle().spawn(async move {
        let end = match download_inner(rel, &tx).await {
            Ok(path) => InstallEvent::Done(path),
            Err(e) => InstallEvent::Error(e.message().to_string()),
        };
        let _ = tx.send(end);
    });
    rx
}

/// The setup's command line: silent with `/update=1` for an installed
/// copy (it waits for this process, installs, restarts the app), the
/// normal wizard for a portable one.
fn installer_args(kind: InstallKind) -> Vec<&'static str> {
    match kind {
        InstallKind::Installed => vec!["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-", "/update=1"],
        _ => vec!["/SP-"],
    }
}

/// Start the downloaded installer; the caller quits right after.
pub fn run_installer(setup: &Path) -> Result<(), DbError> {
    std::process::Command::new(setup)
        .args(installer_args(install_kind()))
        .spawn()
        .map_err(|e| DbError::with_source(format!("cannot start {}: {e}", setup.display()), e))?;
    Ok(())
}

/// Hold the named instance mutex for the life of the process: the
/// installer (`installer/databridge.iss`) waits until it is gone before
/// replacing the exe, and asks the user to close the app otherwise.
pub fn hold_instance_mutex() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let name: Vec<u16> = INSTANCE_MUTEX.encode_utf16().chain(Some(0)).collect();
        // The handle is never closed: the OS releases it when we exit.
        unsafe {
            CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        }
    }
}

/// Remove what earlier updates left behind: the `.old` exe of a 0.3 / 0.4
/// in-place swap, a broken `.download`, and installers already run.
/// Called once at startup; failures are harmless.
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(sibling(&exe, ".old"));
        let _ = std::fs::remove_file(sibling(&exe, ".download"));
    }
    let _ = std::fs::remove_dir_all(download_dir());
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
        let sum = format!("{SETUP_ASSET}.sha256");
        let legacy_sum = format!("{LEGACY_ASSET}.sha256");
        let r = newer_release(
            gh("v0.5.0", &["x.zip", SETUP_ASSET, &sum, LEGACY_ASSET, &legacy_sum]),
            "0.4.0",
        )
        .unwrap();
        assert_eq!(r.version, "0.5.0");
        assert_eq!(r.setup_url.as_deref(), Some(format!("https://dl/{SETUP_ASSET}").as_str()));
        assert_eq!(r.sha256_url.as_deref(), Some(format!("https://dl/{sum}").as_str()));
        // Releases without an installer are offered as a page link only.
        let r = newer_release(gh("v0.4.0", &["x.zip", LEGACY_ASSET, &legacy_sum]), "0.2.0").unwrap();
        assert!(r.setup_url.is_none() && r.sha256_url.is_none() && !r.installable());
        assert!(newer_release(gh("v0.2.0", &[SETUP_ASSET]), "0.2.0").is_none());
        let mut pre = gh("v0.6.0", &[SETUP_ASSET]);
        pre.prerelease = true;
        assert!(newer_release(pre, "0.2.0").is_none());
    }

    #[test]
    fn checksum_file_formats() {
        let h = "a".repeat(64);
        assert_eq!(parse_checksum(&h), Some(h.clone()));
        assert_eq!(parse_checksum(&format!("{}  {SETUP_ASSET}\n", h.to_uppercase())), Some(h.clone()));
        assert_eq!(parse_checksum("deadbeef  x.exe"), None);
        assert_eq!(parse_checksum(""), None);
    }

    #[test]
    fn installed_copies_update_silently_portable_ones_install() {
        let dir = std::env::temp_dir().join(format!("databridge-kind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(kind_of_dir(&dir), InstallKind::Portable);
        std::fs::write(dir.join("unins000.exe"), b"").unwrap();
        assert_eq!(kind_of_dir(&dir), InstallKind::Installed);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(installer_args(InstallKind::Installed).contains(&"/update=1"));
        assert!(installer_args(InstallKind::Installed).contains(&"/VERYSILENT"));
        assert!(!installer_args(InstallKind::Portable).iter().any(|a| a.contains("SILENT")));
    }
}
