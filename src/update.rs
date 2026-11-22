//! Checking GitHub for a newer release and installing it (Windows installer).
//!
//! Network access goes through the `curl` program that ships with Windows 10/11 and
//! Linux, so magic-pdf needs no HTTP or TLS code of its own. Only the public release
//! information of the magic-pdf repository is requested; nothing about the user is
//! sent. Downloaded installers are verified against the SHA-256 digest GitHub
//! publishes for each release file before they are run.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use sha2::{Digest, Sha256};

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/magicmatt007/magic-pdf/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/magicmatt007/magic-pdf/releases/latest";

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
    /// "sha256:<hex>"
    pub digest: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn version(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }

    /// The installer for this platform: the Windows setup program or the macOS disk
    /// image.
    pub fn installer(&self) -> Option<&Asset> {
        let suffix = if cfg!(target_os = "macos") { "-macos.dmg" } else { "-setup.exe" };
        self.assets.iter().find(|a| a.name.ends_with(suffix))
    }
}

/// Compares dotted version numbers ("0.10.2" > "0.9.9"). Non-numeric parts count as 0.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['.', '-', '+'])
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parse(candidate), parse(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

fn curl() -> Command {
    let mut cmd = Command::new("curl");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.args(["--fail", "--silent", "--show-error", "--location", "--max-time", "120"]);
    cmd
}

/// Asks GitHub for the latest release.
pub fn latest_release() -> Result<Release, String> {
    let out = curl()
        .args(["-H", "Accept: application/vnd.github+json", "-A", "magic-pdf-update-check"])
        .arg(LATEST_RELEASE_API)
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("unexpected answer from GitHub: {e}"))
}

/// The newest release if it is newer than this build.
pub fn check() -> Result<Option<Release>, String> {
    let release = latest_release()?;
    Ok(is_newer(release.version(), env!("CARGO_PKG_VERSION")).then_some(release))
}

/// Checks a file against a GitHub digest ("sha256:<hex>").
pub fn verify(path: &Path, digest: &str) -> Result<(), String> {
    let expected = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| format!("unsupported digest {digest}"))?
        .to_ascii_lowercase();
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let actual: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    if actual == expected {
        Ok(())
    } else {
        Err("the download is damaged or was tampered with (checksum mismatch)".into())
    }
}

/// Downloads and verifies the installer; returns its path.
pub fn download_installer(asset: &Asset) -> Result<PathBuf, String> {
    let digest = asset
        .digest
        .as_deref()
        .ok_or("GitHub did not publish a checksum for the installer")?;
    let dir = std::env::temp_dir().join("magic-pdf-update");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(&asset.name);
    let status = curl()
        .arg("--output")
        .arg(&path)
        .arg(&asset.browser_download_url)
        .status()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !status.success() {
        return Err("the download failed".into());
    }
    if let Err(e) = verify(&path, digest) {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    Ok(path)
}

/// Starts the installer. Windows: updates in place and restarts magic-pdf. macOS:
/// opens the disk image so the new app can be dragged to Applications.
pub fn run_installer(path: &Path) -> Result<(), String> {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(path);
        c
    } else {
        let mut c = Command::new(path);
        c.args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART"]);
        c
    };
    cmd.spawn().map(|_| ()).map_err(|e| format!("could not start the installer: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("1.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn parses_github_release_and_picks_installer() {
        let json = r#"{"tag_name":"v0.2.0","name":"magic-pdf 0.2.0","body":"Notes","html_url":"https://x/r",
            "assets":[{"name":"magic-pdf-0.2.0-windows-portable.zip","browser_download_url":"https://x/z","size":2,"digest":"sha256:ab"},
                      {"name":"magic-pdf-0.2.0-setup.exe","browser_download_url":"https://x/s","size":1,"digest":null},
                      {"name":"magic-pdf-0.2.0-macos.dmg","browser_download_url":"https://x/m","size":3,"digest":null}],
            "extra":"ignored"}"#;
        let r: Release = serde_json::from_str(json).unwrap();
        assert_eq!(r.version(), "0.2.0");
        let expected = if cfg!(target_os = "macos") { "https://x/m" } else { "https://x/s" };
        assert_eq!(r.installer().unwrap().browser_download_url, expected);
        assert_eq!(r.installer().unwrap().digest, None);
    }

    #[test]
    fn verifies_checksums() {
        let path = std::env::temp_dir().join(format!("magic-pdf-verify-{}", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();
        let good = "sha256:2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824";
        assert!(verify(&path, good).is_ok());
        assert!(verify(&path, "sha256:00").is_err());
        assert!(verify(&path, "md5:00").is_err());
        std::fs::remove_file(path).ok();
    }
}

#[cfg(test)]
mod live {
    /// Talks to GitHub; run with `cargo test live_release -- --ignored`.
    #[test]
    #[ignore]
    fn live_release() {
        let r = super::latest_release().unwrap();
        eprintln!("latest: {} installer: {:?}", r.tag_name, r.installer().map(|a| (&a.name, &a.digest)));
        assert!(r.installer().is_some_and(|a| a.digest.is_some()));
        eprintln!("update available for this build: {}", super::check().unwrap().is_some());
        let path = super::download_installer(r.installer().unwrap()).unwrap();
        eprintln!("downloaded and verified: {} ({} bytes)", path.display(), std::fs::metadata(&path).unwrap().len());
        std::fs::remove_file(path).ok();
    }
}
