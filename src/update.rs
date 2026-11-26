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

/// One block of a release's "What's new" text (a small part of Markdown).
#[derive(Clone, Debug, PartialEq)]
pub enum NoteBlock {
    /// A line that is bold as a whole: "**Tabs**".
    Heading(String),
    /// "- text", including its wrapped continuation lines.
    Bullet(String),
    Paragraph(String),
}

/// The "What's new" part of a release text: everything after a "## What's new" heading
/// up to the next "## " heading (the installation notes). None when the release has
/// no such part.
pub fn whats_new(body: &str) -> Option<Vec<NoteBlock>> {
    let mut lines = body.lines().skip_while(|l| l.trim().is_empty());
    if !lines.next()?.trim_start().starts_with("## What's new") {
        return None;
    }
    let mut blocks: Vec<NoteBlock> = Vec::new();
    let mut open = false;
    for line in lines.take_while(|l| !l.starts_with("## ")) {
        let text = line.trim();
        if text.is_empty() {
            open = false;
        } else if let Some(item) = text.strip_prefix("- ").or_else(|| text.strip_prefix("* ")) {
            blocks.push(NoteBlock::Bullet(item.trim().to_string()));
            open = true;
        } else if !open && text.len() > 4 && text.starts_with("**") && text.ends_with("**") && !text[2..text.len() - 2].contains("**") {
            blocks.push(NoteBlock::Heading(text[2..text.len() - 2].to_string()));
        } else if open && let Some(NoteBlock::Bullet(t) | NoteBlock::Paragraph(t)) = blocks.last_mut() {
            t.push(' ');
            t.push_str(text);
        } else {
            blocks.push(NoteBlock::Paragraph(text.to_string()));
            open = true;
        }
    }
    (!blocks.is_empty()).then_some(blocks)
}

/// Splits "a **b** `c`" into text parts and whether each is bold; code marks are dropped.
pub fn bold_parts(text: &str) -> Vec<(String, bool)> {
    text.replace('`', "")
        .split("**")
        .enumerate()
        .filter(|(_, part)| !part.is_empty())
        .map(|(i, part)| (part.to_string(), i % 2 == 1))
        .collect()
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

/// A copy installed with packaging/linux/install.sh that this user may replace:
/// `<prefix>/bin/magic-pdf` with PDFium in `<prefix>/lib/magic-pdf`. Returns the
/// program and the library. Builds run from `target/` don't qualify.
fn installed_copy() -> Option<(PathBuf, PathBuf)> {
    if !cfg!(target_os = "linux") || running_appimage().is_some() {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    // Linux names a program that was replaced while it runs "… (deleted)".
    let exe = match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(s) => PathBuf::from(s),
        None => exe,
    };
    installed_layout(&exe)
}

fn installed_layout(exe: &Path) -> Option<(PathBuf, PathBuf)> {
    let bin_dir = exe.parent()?;
    if bin_dir.file_name()? != "bin" {
        return None;
    }
    let lib = bin_dir.parent()?.join("lib/magic-pdf/libpdfium.so");
    (lib.is_file() && writable(bin_dir) && writable(lib.parent()?)).then(|| (exe.to_path_buf(), lib))
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".magic-pdf-write-test-{}", std::process::id()));
    let ok = std::fs::File::create(&probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Whether this build can install updates itself: always on Windows and macOS; on
/// Linux when running from an AppImage or installed with install.sh (then the
/// release's AppImage is unpacked and its program and PDFium replace the installed
/// ones). Builds run straight from the source folder are updated with git.
pub fn can_install() -> bool {
    cfg!(any(windows, target_os = "macos"))
        || (cfg!(target_os = "linux")
            && cfg!(target_arch = "x86_64")
            && (running_appimage().is_some() || installed_copy().is_some()))
}

/// Where a download goes: next to the running AppImage on Linux (so it can replace it
/// with an atomic rename on the same file system), next to the installed PDFium for a
/// copy installed with install.sh, the temp directory elsewhere.
fn download_path(asset: &Asset) -> Result<PathBuf, String> {
    if cfg!(target_os = "linux")
        && let Some(appimage) = running_appimage()
    {
        let dir = appimage.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        return Ok(dir.join(format!(".{}.part", asset.name)));
    }
    if let Some((_, lib)) = installed_copy() {
        let dir = lib.with_file_name(".update");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        return Ok(dir.join(&asset.name));
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
    cmd.args(["--fail", "--silent", "--show-error", "--location", "--connect-timeout", "30"]);
    cmd
}

/// Asks GitHub for the latest release.
pub fn latest_release() -> Result<Release, String> {
    let out = curl()
        .args(["--max-time", "60", "-H", "Accept: application/vnd.github+json", "-A", "magic-pdf-update-check"])
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
    // No overall time limit (a slow connection still gets there); a download that
    // stalls (under 1 kB/s for a minute) is given up.
    let status = curl()
        .args(["--speed-limit", "1024", "--speed-time", "60"])
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
/// the running AppImage with the downloaded one (or, for a copy installed with
/// install.sh, the program and PDFium with the ones inside it) and starts it.
pub fn run_installer(path: &Path) -> Result<(), String> {
    if cfg!(target_os = "linux") {
        let program = if let Some(appimage) = running_appimage() {
            replace_file(path, &appimage, true)?;
            appimage
        } else {
            let (program, lib) = installed_copy().ok_or("not running from an AppImage or an installed copy")?;
            let result = install_from_appimage(path, &program, &lib);
            if let Some(dir) = path.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
            result?;
            program
        };
        return Command::new(&program)
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

/// Unpacks the downloaded AppImage next to it (`--appimage-extract`, no FUSE needed)
/// and moves its program and PDFium over the installed ones.
fn install_from_appimage(appimage: &Path, program: &Path, lib: &Path) -> Result<(), String> {
    let dir = appimage.parent().ok_or("no folder for unpacking")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(appimage, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    let status = Command::new(appimage)
        .arg("--appimage-extract")
        .current_dir(dir)
        .env_remove("APPIMAGE_EXTRACT_AND_RUN")
        .stdout(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("could not unpack the update: {e}"))?;
    let root = dir.join("squashfs-root");
    let (new_program, new_lib) = (root.join("usr/bin/magic-pdf"), root.join("usr/lib/magic-pdf/libpdfium.so"));
    if !status.success() || !new_program.is_file() || !new_lib.is_file() {
        return Err("could not unpack the update".into());
    }
    // PDFium first: a new program always comes with the PDFium it was built against.
    replace_file(&new_lib, lib, false)?;
    replace_file(&new_program, program, true)
}

/// Moves `new` over `target` in one step (`rename` within one file system is atomic,
/// so `target` is never missing or half written), keeping `target`'s permissions and
/// making it executable if asked. A running program keeps using the old file until
/// it exits.
fn replace_file(new: &Path, target: &Path, executable: bool) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let default = if executable { 0o755 } else { 0o644 };
        let mode = std::fs::metadata(target).map(|m| m.permissions().mode() & 0o777).unwrap_or(default);
        let mode = if executable { mode | 0o111 } else { mode };
        std::fs::set_permissions(new, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("could not set the permissions of the update: {e}"))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
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
        replace_file(&new, &old, true).unwrap();
        assert_eq!(std::fs::read(&old).unwrap(), b"new");
        assert_eq!(std::fs::metadata(&old).unwrap().permissions().mode() & 0o777, 0o751);
        assert!(!new.exists());
        std::fs::remove_dir_all(dir).ok();
    }

    /// A copy installed with install.sh is updated from the files inside the AppImage.
    #[cfg(target_os = "linux")]
    #[test]
    fn updates_an_installed_copy_from_an_appimage() {
        use std::os::unix::fs::PermissionsExt;
        let prefix = std::env::temp_dir().join(format!("magic-pdf-installed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&prefix);
        let (program, lib) = (prefix.join("bin/magic-pdf"), prefix.join("lib/magic-pdf/libpdfium.so"));
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        std::fs::create_dir_all(lib.parent().unwrap()).unwrap();
        std::fs::write(&program, "old program").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&lib, "old pdfium").unwrap();
        std::fs::set_permissions(&lib, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(installed_layout(&program), Some((program.clone(), lib.clone())));
        assert_eq!(installed_layout(&prefix.join("target/release/magic-pdf")), None);

        // Stands in for the AppImage: unpacks like `--appimage-extract` does.
        let update = lib.with_file_name(".update");
        std::fs::create_dir_all(&update).unwrap();
        let appimage = update.join("magic-pdf-9.9.9-x86_64.AppImage");
        std::fs::write(
            &appimage,
            "#!/bin/sh\n[ \"$1\" = --appimage-extract ] || exit 1\n\
             mkdir -p squashfs-root/usr/bin squashfs-root/usr/lib/magic-pdf\n\
             echo new program > squashfs-root/usr/bin/magic-pdf\n\
             echo new pdfium > squashfs-root/usr/lib/magic-pdf/libpdfium.so\n",
        )
        .unwrap();
        install_from_appimage(&appimage, &program, &lib).unwrap();
        assert_eq!(std::fs::read_to_string(&program).unwrap(), "new program\n");
        assert_eq!(std::fs::read_to_string(&lib).unwrap(), "new pdfium\n");
        assert_eq!(std::fs::metadata(&program).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(std::fs::metadata(&lib).unwrap().permissions().mode() & 0o777, 0o644);
        std::fs::remove_dir_all(prefix).ok();
    }

    #[test]
    fn release_text_shows_only_whats_new() {
        let body = "## What's new in 0.7.4\n\nThis release collects\nthe fixes.\n\n**Tabs**\n\
            - On Windows, dragging **works**:\n  let go outside.\n- Second `item`.\n\n\
            ## Windows\n\nDownload the installer.\n";
        assert_eq!(
            whats_new(body).unwrap(),
            [
                NoteBlock::Paragraph("This release collects the fixes.".into()),
                NoteBlock::Heading("Tabs".into()),
                NoteBlock::Bullet("On Windows, dragging **works**: let go outside.".into()),
                NoteBlock::Bullet("Second `item`.".into()),
            ]
        );
        // Releases without a "What's new" part (only installation notes).
        assert_eq!(whats_new("## Windows\n\nDownload it."), None);
        assert_eq!(whats_new("## What's new in 1.0\n\n## Windows\n"), None);
        assert_eq!(
            bold_parts("dragging **works**, see `x`"),
            [("dragging ".to_string(), false), ("works".to_string(), true), (", see x".to_string(), false)]
        );
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
        // Linux: update a copy installed with install.sh (in a throwaway prefix).
        if cfg!(target_os = "linux") {
            let prefix = std::env::temp_dir().join(format!("magic-pdf-live-{}", std::process::id()));
            let (program, lib) = (prefix.join("bin/magic-pdf"), prefix.join("lib/magic-pdf/libpdfium.so"));
            std::fs::create_dir_all(program.parent().unwrap()).unwrap();
            std::fs::create_dir_all(lib.parent().unwrap()).unwrap();
            std::fs::write(&program, "old").unwrap();
            std::fs::write(&lib, "old").unwrap();
            let update = lib.with_file_name(".update");
            std::fs::create_dir_all(&update).unwrap();
            let appimage = update.join(path.file_name().unwrap());
            std::fs::copy(&path, &appimage).unwrap();
            super::install_from_appimage(&appimage, &program, &lib).unwrap();
            let size = |p: &std::path::Path| std::fs::metadata(p).unwrap().len();
            eprintln!("installed: program {} bytes, PDFium {} bytes", size(&program), size(&lib));
            assert!(size(&program) > 1_000_000 && size(&lib) > 1_000_000);
            std::fs::remove_dir_all(prefix).ok();
        }
        std::fs::remove_file(path).ok();
    }
}
