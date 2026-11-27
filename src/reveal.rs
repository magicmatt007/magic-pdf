//! "Open containing folder": shows a file in the system's file manager, selected.

use std::path::Path;

/// Opens the folder that contains `path` in the file manager, with the file selected
/// where the platform supports it. Runs in the background.
pub fn show_in_folder(path: &Path) {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        if let Err(e) = reveal(&path) {
            log::warn!("could not show {} in its folder: {e}", path.display());
            // At least open the folder.
            if let Some(dir) = path.parent()
                && let Err(e) = open::that_detached(dir)
            {
                log::warn!("could not open {}: {e}", dir.display());
            }
        }
    });
}

#[cfg(windows)]
fn reveal(path: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    // Explorer wants the path quoted after the comma, which normal argument quoting
    // does not do.
    std::process::Command::new("explorer").raw_arg(format!("/select,\"{}\"", path.display())).spawn().map(|_| ())
}

#[cfg(target_os = "macos")]
fn reveal(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("open").arg("-R").arg(path).spawn().map(|_| ())
}

/// The freedesktop file manager interface (Nautilus, Dolphin, Nemo, Thunar, …).
#[cfg(not(any(windows, target_os = "macos")))]
fn reveal(path: &Path) -> std::io::Result<()> {
    let status = std::process::Command::new("dbus-send")
        .args([
            "--session",
            "--print-reply",
            "--dest=org.freedesktop.FileManager1",
            "--type=method_call",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1.ShowItems",
        ])
        .arg(format!("array:string:{}", file_uri(path)))
        .arg("string:")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() { Ok(()) } else { Err(std::io::Error::other("no file manager answered")) }
}

/// A file:// URI for an absolute path, with everything but unreserved characters
/// percent-encoded (commas too, as dbus-send splits arrays at them).
#[cfg(not(any(windows, target_os = "macos")))]
fn file_uri(path: &Path) -> String {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = String::from("file://");
    for b in abs.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(b as char);
        } else {
            out += &format!("%{b:02X}");
        }
    }
    out
}

#[cfg(all(test, not(any(windows, target_os = "macos"))))]
mod tests {
    use super::*;

    #[test]
    fn file_uris_are_percent_encoded() {
        assert_eq!(
            file_uri(Path::new("/home/me/a b,c/Bericht ä.pdf")),
            "file:///home/me/a%20b%2Cc/Bericht%20%C3%A4.pdf"
        );
    }
}
