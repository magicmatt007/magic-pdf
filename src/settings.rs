//! Persisted user preferences, recent files and per-file view state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::engine::{Rgb, Rotation};
use crate::view::layout::LayoutMode;
use crate::view::viewport::FitMode;

const MAX_RECENT: usize = 15;
const MAX_FILE_STATES: usize = 300;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileState {
    pub page: usize,
    pub zoom: f32,
    pub fit: FitMode,
    pub mode: LayoutMode,
    pub rotation: Rotation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredFileState {
    #[serde(flatten)]
    state: FileState,
    last_used: u64,
}

/// Default colours and widths for new annotations.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AnnotStyle {
    pub highlight: Rgb,
    pub underline: Rgb,
    pub strikeout: Rgb,
    pub note: Rgb,
    pub ink: Rgb,
    pub ink_width: f32,
    pub shape: Rgb,
    pub shape_width: f32,
    pub shape_fill: Option<Rgb>,
    pub textbox_border: Rgb,
    pub textbox_border_width: f32,
    pub textbox_text: Rgb,
    pub textbox_font_size: f32,
    pub textbox_fill: Option<Rgb>,
    /// Stamp tool: the chosen stamp (a text, or an image when `stamp_image` is set).
    pub stamp_text: String,
    pub stamp_color: Rgb,
    /// Add "author, date" under the stamp text.
    pub stamp_details: bool,
    pub stamp_image: Option<PathBuf>,
    /// Stamps the user made: texts and image files.
    pub custom_stamps: Vec<String>,
    pub image_stamps: Vec<PathBuf>,
    /// Signature tool: the chosen saved signature and the ink colour of drawn ones.
    pub signature: usize,
    pub signature_color: Rgb,
}

/// A saved signature.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Signature {
    /// Drawn with the mouse: strokes with x in 0..1 and y downwards in 0..height/width.
    Drawn(Vec<Vec<[f32; 2]>>),
    /// A picture file (e.g. a scanned signature).
    Image(PathBuf),
}

/// The built-in stamps and their colours.
pub const STAMPS: [(&str, Rgb); 9] = [
    ("APPROVED", [0, 140, 60]),
    ("REVIEWED", [0, 90, 200]),
    ("FINAL", [0, 140, 60]),
    ("DRAFT", [0, 90, 200]),
    ("FOR COMMENT", [0, 90, 200]),
    ("CONFIDENTIAL", [200, 0, 0]),
    ("NOT APPROVED", [200, 0, 0]),
    ("VOID", [200, 0, 0]),
    ("COMPLETED", [0, 140, 60]),
];

impl Default for AnnotStyle {
    fn default() -> Self {
        Self {
            highlight: [255, 230, 0],
            underline: [0, 110, 230],
            strikeout: [220, 30, 30],
            note: [255, 210, 0],
            ink: [220, 30, 30],
            ink_width: 2.0,
            shape: [220, 30, 30],
            shape_width: 2.0,
            shape_fill: None,
            textbox_border: [220, 30, 30],
            textbox_border_width: 1.0,
            textbox_text: [0, 0, 0],
            textbox_font_size: 12.0,
            textbox_fill: Some([255, 255, 255]),
            stamp_text: "APPROVED".into(),
            stamp_color: [0, 140, 60],
            stamp_details: true,
            stamp_image: None,
            custom_stamps: Vec::new(),
            image_stamps: Vec::new(),
            signature: 0,
            signature_color: [10, 40, 140],
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub default_mode: LayoutMode,
    pub show_sidebar: bool,
    /// Width of the sidebar in points.
    pub sidebar_width: f32,
    /// Preferred thumbnail width in points (several columns when the sidebar is wide).
    pub thumb_width: f32,
    pub night_mode: bool,
    pub dark_ui: bool,
    pub recent: Vec<PathBuf>,
    /// Stored as the author (/T) of new annotations.
    pub author: String,
    pub annot_style: AnnotStyle,
    /// Look for a new release at most once a day.
    pub check_updates: bool,
    /// Unix time of the last update check.
    pub last_update_check: u64,
    /// A release the user chose to skip.
    pub skipped_version: Option<String>,
    /// Colours picked most recently (shown in the colour picker).
    pub recent_colors: Vec<Rgb>,
    /// Reopen the files of the last window closed when magic-pdf starts without files.
    pub restore_session: bool,
    /// Files open when the last window was closed, and the active one.
    pub session: Vec<PathBuf>,
    pub session_active: usize,
    /// Recent files kept at the top of the list (never pushed out).
    pub pinned: Vec<PathBuf>,
    pub signatures: Vec<Signature>,
    files: HashMap<String, StoredFileState>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_mode: LayoutMode::Horizontal,
            show_sidebar: true,
            sidebar_width: 280.0,
            thumb_width: 150.0,
            night_mode: false,
            dark_ui: true,
            recent: Vec::new(),
            author: default_author(),
            annot_style: AnnotStyle::default(),
            check_updates: true,
            last_update_check: 0,
            skipped_version: None,
            recent_colors: Vec::new(),
            restore_session: true,
            session: Vec::new(),
            session_active: 0,
            pinned: Vec::new(),
            signatures: Vec::new(),
            files: HashMap::new(),
        }
    }
}

fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "magic-pdf").map(|d| d.config_dir().join("settings.toml"))
}

fn default_author() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Settings {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                log::warn!("ignoring invalid settings {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("toml.tmp");
            std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
            std::fs::rename(&tmp, &path)?;
            Ok(())
        })();
        if let Err(e) = result {
            log::warn!("failed to save settings to {}: {e}", path.display());
        }
    }

    /// Pins a recent file to the top of the list, or unpins it.
    pub fn toggle_pin(&mut self, path: &Path) {
        if let Some(i) = self.pinned.iter().position(|p| p == path) {
            self.pinned.remove(i);
            self.add_recent(path);
        } else {
            self.pinned.push(path.to_path_buf());
            self.recent.retain(|p| p != path);
        }
    }

    /// Removes a file from the recent and pinned lists.
    pub fn forget_recent(&mut self, path: &Path) {
        self.recent.retain(|p| p != path);
        self.pinned.retain(|p| p != path);
    }

    pub fn add_recent(&mut self, path: &Path) {
        if self.pinned.iter().any(|p| p == path) {
            return;
        }
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_path_buf());
        self.recent.truncate(MAX_RECENT);
    }

    pub fn file_state(&self, path: &Path) -> Option<&FileState> {
        self.files.get(&key(path)).map(|s| &s.state)
    }

    pub fn set_file_state(&mut self, path: &Path, state: FileState) {
        self.files.insert(
            key(path),
            StoredFileState {
                state,
                last_used: now(),
            },
        );
        if self.files.len() > MAX_FILE_STATES {
            let mut by_age: Vec<(String, u64)> =
                self.files.iter().map(|(k, v)| (k.clone(), v.last_used)).collect();
            by_age.sort_by_key(|e| e.1);
            for (k, _) in by_age.into_iter().take(self.files.len() - MAX_FILE_STATES) {
                self.files.remove(&k);
            }
        }
    }
}

fn key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_files_stay_at_the_top_and_out_of_the_recent_list() {
        let mut s = Settings::default();
        let (a, b, c) = (Path::new("/a.pdf"), Path::new("/b.pdf"), Path::new("/c.pdf"));
        s.add_recent(a);
        s.add_recent(b);
        s.toggle_pin(a);
        assert_eq!(s.pinned, vec![a.to_path_buf()]);
        assert_eq!(s.recent, vec![b.to_path_buf()]);
        // Opening a pinned file does not add it to the recent list.
        s.add_recent(a);
        assert_eq!(s.recent, vec![b.to_path_buf()]);
        for i in 0..MAX_RECENT + 3 {
            s.add_recent(Path::new(&format!("/{i}.pdf")));
        }
        assert_eq!(s.pinned, vec![a.to_path_buf()], "never pushed out");
        // Unpinning puts it back at the top of the recent files.
        s.toggle_pin(a);
        assert!(s.pinned.is_empty());
        assert_eq!(s.recent[0], a);
        s.add_recent(c);
        s.forget_recent(c);
        assert!(!s.recent.iter().any(|p| p == c));
    }

    #[test]
    fn old_settings_files_get_the_new_defaults() {
        let s: Settings = toml::from_str("show_sidebar = false\n").unwrap();
        assert!(!s.show_sidebar);
        assert!(s.restore_session);
        assert!(s.session.is_empty() && s.pinned.is_empty());
    }
}
