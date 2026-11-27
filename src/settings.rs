//! Persisted user preferences, recent files and per-file view state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::engine::{Rgb, Rotation};
use crate::view::layout::LayoutMode;
use crate::view::viewport::FitMode;

/// Files kept in the history of recently opened files (pinned ones come on top).
const MAX_RECENT: usize = 100;
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
    /// Measuring: drawing scale 1:`measure_ratio`, results in `measure_unit`.
    pub measure_ratio: f32,
    pub measure_unit: String,
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
            measure_ratio: 1.0,
            measure_unit: "mm".into(),
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
    /// Digital signatures: the certificate file last used (never its password).
    pub sign_certificate: Option<PathBuf>,
    /// Windows: sign with a certificate from the Windows certificate store (its
    /// thumbprint in `sign_store_certificate`) instead of a certificate file.
    pub sign_from_store: bool,
    pub sign_store_certificate: Option<String>,
    /// Add a trusted timestamp from `timestamp_server` when signing.
    pub sign_timestamp: bool,
    pub sign_reason: String,
    pub sign_location: String,
    /// Picture for digital signatures: an index into `signatures`, or none (text only).
    pub sign_picture: Option<usize>,
    /// RFC 3161 timestamp server.
    pub timestamp_server: String,
    /// User interface language: "auto" (the system's), "en" or "de".
    pub language: String,
    files: HashMap<String, StoredFileState>,
    /// The settings as this window last read or wrote them. Saving writes back only
    /// what changed since, so windows (processes) do not undo each other's changes.
    #[serde(skip)]
    base: Option<toml::Value>,
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
            restore_session: false,
            session: Vec::new(),
            session_active: 0,
            pinned: Vec::new(),
            signatures: Vec::new(),
            sign_certificate: None,
            sign_from_store: cfg!(windows),
            sign_store_certificate: None,
            sign_timestamp: true,
            sign_reason: String::new(),
            sign_location: String::new(),
            sign_picture: Some(0),
            timestamp_server: "http://timestamp.digicert.com".into(),
            language: "auto".into(),
            files: HashMap::new(),
            base: None,
        }
    }
}

/// Three-way merge of settings: where `mine` differs from `base` (what this window
/// changed) it wins, elsewhere `disk` (what other windows saved) is kept. Tables are
/// merged key by key, other values as a whole.
fn merge3(base: &toml::Value, mine: &toml::Value, disk: &toml::Value) -> toml::Value {
    use toml::Value;
    match (base, mine, disk) {
        (Value::Table(b), Value::Table(m), Value::Table(d)) => {
            let mut out = toml::Table::new();
            let keys: std::collections::BTreeSet<&String> = m.keys().chain(d.keys()).collect();
            for k in keys {
                let value = match (b.get(k), m.get(k), d.get(k)) {
                    (Some(bv), Some(mv), Some(dv)) => Some(merge3(bv, mv, dv)),
                    // Unchanged here (or gone here and there): take the file's.
                    (bv, mv, dv) if bv == mv => dv.cloned(),
                    // Changed (added or removed) here.
                    (_, mv, _) => mv.cloned(),
                };
                if let Some(v) = value {
                    out.insert(k.clone(), v);
                }
            }
            Value::Table(out)
        }
        _ if mine == base => disk.clone(),
        _ => mine.clone(),
    }
}

fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "magic-pdf").map(|d| d.config_dir().join("settings.toml"))
}

fn default_author() -> String {
    std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default()
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Settings {
    pub fn load() -> Self {
        match config_path() {
            Some(path) => Self::load_from(&path),
            None => Self::default(),
        }
    }

    fn load_from(path: &Path) -> Self {
        let mut s = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                // Kept for recovery instead of being overwritten by the defaults.
                let backup = path.with_extension("toml.bad");
                let _ = std::fs::copy(path, &backup);
                log::warn!("ignoring invalid settings {} (copy kept as {}): {e}", path.display(), backup.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        s.base = toml::Value::try_from(&s).ok();
        s
    }

    /// Saves the settings, merged with changes other windows saved meanwhile: what
    /// this window changed since it last read or wrote the file wins, everything else
    /// is taken from the file. This window then has the merged settings.
    pub fn save(&mut self) {
        if let Some(path) = config_path() {
            self.save_to(&path);
        }
    }

    fn save_to(&mut self, path: &Path) {
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mine = toml::Value::try_from(&*self)?;
            let disk =
                std::fs::read_to_string(path).ok().and_then(|t| t.parse::<toml::Table>().ok()).map(toml::Value::Table);
            let merged = match (&self.base, disk) {
                (Some(base), Some(disk)) => merge3(base, &mine, &disk),
                _ => mine,
            };
            // Per process, so two windows saving at once cannot mix their files.
            let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
            std::fs::write(&tmp, toml::to_string_pretty(&merged)?)?;
            if let Err(e) = std::fs::rename(&tmp, path) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
            if let Ok(s) = merged.clone().try_into::<Settings>() {
                *self = s;
            }
            self.base = Some(merged);
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
        self.files.insert(key(path), StoredFileState { state, last_used: now() });
        if self.files.len() > MAX_FILE_STATES {
            let mut by_age: Vec<(String, u64)> = self.files.iter().map(|(k, v)| (k.clone(), v.last_used)).collect();
            by_age.sort_by_key(|e| e.1);
            for (k, _) in by_age.into_iter().take(self.files.len() - MAX_FILE_STATES) {
                self.files.remove(&k);
            }
        }
    }
}

fn key(path: &Path) -> String {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_do_not_undo_each_others_settings() {
        let dir = std::env::temp_dir().join(format!("magic-pdf-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("settings.toml");
        // Two windows started with the same settings.
        let mut a = Settings::load_from(&path);
        let mut b = Settings::load_from(&path);
        a.author = "Ann".into();
        a.signatures.push(Signature::Drawn(vec![vec![[0.0, 0.0], [1.0, 0.5]]]));
        a.save_to(&path);
        // The other window changes something else and saves later with its old copy.
        b.dark_ui = !b.dark_ui;
        b.save_to(&path);
        let now = Settings::load_from(&path);
        assert_eq!(now.author, "Ann", "the first window's change is kept");
        assert_eq!(now.signatures.len(), 1, "the signature drawn in the first window is kept");
        assert_eq!(now.dark_ui, b.dark_ui, "the second window's change is saved");
        assert_eq!(b.author, "Ann", "the saving window now has the other's change");
        // A change made here wins over the file for that setting only.
        a.author = "Anna".into();
        a.save_to(&path);
        let now = Settings::load_from(&path);
        assert_eq!((now.author.as_str(), now.dark_ui), ("Anna", b.dark_ui));
        // An unreadable file is kept as a backup, not lost.
        std::fs::write(&path, "this is = = not toml").unwrap();
        let _ = Settings::load_from(&path);
        assert!(path.with_extension("toml.bad").exists());
        assert!(std::fs::read_dir(&dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")));
        std::fs::remove_dir_all(dir).ok();
    }

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
        assert!(!s.restore_session);
        assert!(s.session.is_empty() && s.pinned.is_empty());
    }
}
