//! Self-update from GitHub releases.
//!
//! Checking asks GitHub's API for the latest release. Installing downloads its `aero.exe`,
//! verifies the SHA-256 printed in the release notes, swaps it in next to the running exe
//! (a running exe can be renamed on Windows, just not overwritten), and starts the new
//! version, which waits for this process to exit first. Networking uses Windows' own
//! curl.exe, so Aero needs no HTTP/TLS libraries.

use crate::recorder::hidden_cmd;
use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};

pub const REPO: &str = "WhyiMicro/Aero";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim().trim_start_matches(['v', 'V']);
        let mut it = s.split(['.', '-', '+']).map(|p| p.parse::<u32>().ok());
        Some(Version(it.next()??, it.next()??, it.next().flatten().unwrap_or(0)))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The version of this build. `AERO_PRETEND_VERSION` overrides it, for testing updates.
pub fn current() -> Version {
    std::env::var("AERO_PRETEND_VERSION")
        .ok()
        .and_then(|v| Version::parse(&v))
        .or_else(|| Version::parse(env!("CARGO_PKG_VERSION")))
        .unwrap_or(Version(0, 0, 0))
}

#[derive(Clone, Debug)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub notes: String,
    pub page_url: String,
    pub exe_url: String,
    pub sha256: Option<String>,
}

fn curl() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    format!(r"{root}\System32\curl.exe")
}

/// Asks GitHub for the latest release. `Ok(None)` = this version is already the newest.
pub fn check() -> Result<Option<Release>> {
    // AERO_UPDATE_API points the check at another URL (e.g. a file:// JSON), for testing.
    let url = std::env::var("AERO_UPDATE_API").unwrap_or_else(|_| format!("https://api.github.com/repos/{REPO}/releases/latest"));
    let out = hidden_cmd(&curl())
        .args(["-sSfL", "--max-time", "20", "-H", "Accept: application/vnd.github+json", "-A", "Aero-updater"])
        .arg(url)
        .output()
        .context("couldn't run curl.exe")?;
    if !out.status.success() {
        bail!("couldn't reach GitHub ({})", String::from_utf8_lossy(&out.stderr).trim());
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).context("unexpected reply from GitHub")?;
    let tag = json["tag_name"].as_str().ok_or_else(|| anyhow!("release has no tag"))?.to_string();
    let version = Version::parse(&tag).ok_or_else(|| anyhow!("can't read version \"{tag}\""))?;
    if version <= current() {
        return Ok(None);
    }
    let exe_url = json["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case("aero.exe")))
        .and_then(|a| a["browser_download_url"].as_str())
        .ok_or_else(|| anyhow!("release {tag} has no aero.exe to download"))?
        .to_string();
    let notes = json["body"].as_str().unwrap_or_default().to_string();
    Ok(Some(Release {
        version,
        page_url: json["html_url"].as_str().unwrap_or_default().to_string(),
        sha256: find_sha256(&notes),
        tag,
        notes,
        exe_url,
    }))
}

/// The release notes contain "SHA-256 of `aero.exe`: `<64 hex chars>`".
fn find_sha256(notes: &str) -> Option<String> {
    let after = &notes[notes.find("SHA-256")?..];
    after
        .split(|c: char| !c.is_ascii_hexdigit())
        .find(|w| w.len() == 64)
        .map(|w| w.to_ascii_lowercase())
}

/// True when running straight out of cargo's `target` folder rather than an installed copy.
pub fn is_dev_build() -> bool {
    std::env::current_exe()
        .ok()
        .is_some_and(|p| p.components().any(|c| c.as_os_str().eq_ignore_ascii_case("target")))
}

/// Downloads and verifies the release, then swaps it in place of the running exe.
/// Returns the path to start. The old exe stays as `aero.old.exe` until the next start.
pub fn install(rel: &Release) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding Aero's own location")?;
    let dir = exe.parent().ok_or_else(|| anyhow!("Aero's folder is unknown"))?;
    let new = dir.join("aero.update.exe");
    let old = dir.join("aero.old.exe");

    let out = hidden_cmd(&curl())
        .args(["-sSfL", "--max-time", "300", "-A", "Aero-updater", "-o"])
        .arg(&new)
        .arg(&rel.exe_url)
        .output()
        .context("couldn't run curl.exe")?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&new);
        bail!("download failed ({})", String::from_utf8_lossy(&out.stderr).trim());
    }

    let bytes = std::fs::read(&new).context("reading the download")?;
    if bytes.len() < 100_000 || !bytes.starts_with(b"MZ") {
        let _ = std::fs::remove_file(&new);
        bail!("the download isn't a Windows program");
    }
    match &rel.sha256 {
        Some(expected) => {
            use sha2::Digest;
            let got: String = sha2::Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
            if &got != expected {
                let _ = std::fs::remove_file(&new);
                bail!("the download is corrupted (checksum mismatch), nothing was changed");
            }
        }
        None => crate::info!("release {} lists no SHA-256; installing unverified", rel.tag),
    }

    // Swap: the running exe can be renamed but not overwritten.
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).context("moving the current version aside")?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe); // put the current version back
        return Err(e).context("putting the new version in place");
    }
    crate::info!("installed {} over {}", rel.tag, exe.display());
    Ok(exe)
}

/// Starts `exe` once this process has exited (it waits on our PID; see `main`).
pub fn relaunch(exe: &Path, show_window: bool) -> Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["--wait-pid", &std::process::id().to_string(), "--updated"]);
    if !show_window {
        cmd.arg("--tray");
    }
    cmd.spawn().context("starting the new version")?;
    Ok(())
}

/// Removes the previous version left behind by an update.
pub fn cleanup_old() {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let _ = std::fs::remove_file(dir.join("aero.old.exe"));
        let _ = std::fs::remove_file(dir.join("aero.update.exe"));
    }
}

// ---------------------------------------------------------------- "check once a day" state

fn state_path() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| ".".into()).join("Aero").join("update.txt")
}

/// (unix time of last automatic check, skipped tag)
fn read_state() -> (u64, String) {
    let text = std::fs::read_to_string(state_path()).unwrap_or_default();
    let mut lines = text.lines();
    let last = lines.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0);
    (last, lines.next().unwrap_or_default().trim().to_string())
}

fn write_state(last: u64, skipped: &str) {
    let _ = std::fs::write(state_path(), format!("{last}\n{skipped}\n"));
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// True if a day has passed since the last automatic check (and records this one).
pub fn auto_check_due() -> bool {
    let (last, skipped) = read_state();
    if now().saturating_sub(last) < 24 * 3600 {
        return false;
    }
    write_state(now(), &skipped);
    true
}

pub fn skipped_tag() -> String {
    read_state().1
}

pub fn skip(tag: &str) {
    write_state(read_state().0, tag);
}
