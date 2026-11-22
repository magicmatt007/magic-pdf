//! Shortcut labels for the platform: "Ctrl+S" on Windows and Linux, "⌘S" on macOS
//! (where the shortcuts use the Command key).

pub fn kb(label: &str) -> String {
    if cfg!(target_os = "macos") { mac(label) } else { label.to_string() }
}

fn mac(label: &str) -> String {
    label
        .replace("Ctrl+Shift+", "⇧⌘")
        .replace("Ctrl+", "⌘")
        .replace("Shift+", "⇧")
        .replace("Alt+", "⌥")
}

#[cfg(test)]
mod tests {
    #[test]
    fn mac_labels() {
        assert_eq!(super::mac("Save as…\tCtrl+Shift+S"), "Save as…\t⇧⌘S");
        assert_eq!(super::mac("Ctrl+C / Ctrl+A"), "⌘C / ⌘A");
        assert_eq!(super::mac("F3 / Shift+F3"), "F3 / ⇧F3");
    }
}
