//! Checking GitHub for a newer release and installing it (Windows installer, macOS
//! disk image, Linux AppImage).
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

    /// The installer for this platform: the Windows setup program, the macOS disk
    /// image or the Linux AppImage.
    pub fn installer(&self) -> Option<&Asset> {
        let suffix = if cfg!(target_os = "macos") {
            "-macos.dmg"
        } else if cfg!(target_os = "linux") {
            "-x86_64.AppImage"
        } else {
            "-setup.exe"
        };
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

/// The AppImage this process runs from (set by the AppImage runtime), if any.
fn running_appimage() -> Option<PathBuf> {
    std::env::var_os("APPIMAGE").filter(|p| !p.is_empty()).map(PathBuf::from)
}

/// Whether this build can install updates itself: always on Windows and macOS; on
/// Linux only when running from an AppImage (builds from source are updated with git).
pub fn can_install() -> bool {
    cfg!(any(windows, target_os = "macos"))
        || (cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") && running_appimage().is_some())
}

/// Where a download goes: next to the running AppImage on Linux (so it can replace it
/// with an atomic rename on the same file system), the temp directory elsewhere.
fn download_path(asset: &Asset) -> Result<PathBuf, String> {
    if cfg!(target_os = "linux")
        && let Some(appimage) = running_appimage()
    {
        let dir = appimage.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        return Ok(dir.join(format!(".{}.part", asset.name)));
    }
    let dir = std::env::temp_dir().join("magic-pdf-update");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join(&asset.name))
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
    let path = download_path(asset)?;
    let status = curl()
        .arg("--output")
        .arg(&path)
        .arg(&asset.browser_download_url)
        .status()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(format!("the download to {} failed", path.display()));
    }
    if let Err(e) = verify(&path, digest) {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    Ok(path)
}

/// Starts the installer. Windows: updates in place and restarts magic-pdf. macOS:
/// opens the disk image so the new app can be dragged to Applications. Linux: replaces
/// the running AppImage with the downloaded one and starts it.
pub fn run_installer(path: &Path) -> Result<(), String> {
    if cfg!(target_os = "linux") {
        let appimage = running_appimage().ok_or("not running from an AppImage")?;
        replace_file(path, &appimage)?;
        return Command::new(&appimage)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("could not start the new version: {e}"));
    }
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

/// Makes `new` executable and moves it over `target` in one step (`rename` within one
/// directory is atomic, so `target` is never missing or half written). A running
/// AppImage keeps using the old file until it exits.
fn replace_file(new: &Path, target: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(target).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o755);
        std::fs::set_permissions(new, std::fs::Permissions::from_mode(mode | 0o111))
            .map_err(|e| format!("could not make the update executable: {e}"))?;
    }
    std::fs::rename(new, target).map_err(|e| format!("could not replace {}: {e}", target.display()))
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
                      {"name":"magic-pdf-0.2.0-macos.dmg","browser_download_url":"https://x/m","size":3,"digest":null},
                      {"name":"magic-pdf-0.2.0-x86_64.AppImage","browser_download_url":"https://x/a","size":4,"digest":null}],
            "extra":"ignored"}"#;
        let r: Release = serde_json::from_str(json).unwrap();
        assert_eq!(r.version(), "0.2.0");
        let expected = if cfg!(target_os = "macos") {
            "https://x/m"
        } else if cfg!(target_os = "linux") {
            "https://x/a"
        } else {
            "https://x/s"
        };
        assert_eq!(r.installer().unwrap().browser_download_url, expected);
        assert_eq!(r.installer().unwrap().digest, None);
    }

    #[cfg(unix)]
    #[test]
    fn replaces_file_atomically_and_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("magic-pdf-replace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (old, new) = (dir.join("app.AppImage"), dir.join(".app.AppImage.part"));
        std::fs::write(&old, b"old").unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o750)).unwrap();
        std::fs::write(&new, b"new").unwrap();
        replace_file(&new, &old).unwrap();
        assert_eq!(std::fs::read(&old).unwrap(), b"new");
        assert_eq!(std::fs::metadata(&old).unwrap().permissions().mode() & 0o777, 0o751);
        assert!(!new.exists());
        std::fs::remove_dir_all(dir).ok();
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
