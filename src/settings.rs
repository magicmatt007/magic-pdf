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
}

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
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub default_mode: LayoutMode,
    pub show_sidebar: bool,
    pub night_mode: bool,
    pub dark_ui: bool,
    pub recent: Vec<PathBuf>,
    /// Stored as the author (/T) of new annotations.
    pub author: String,
    pub annot_style: AnnotStyle,
    files: HashMap<String, StoredFileState>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_mode: LayoutMode::Horizontal,
            show_sidebar: true,
            night_mode: false,
            dark_ui: true,
            recent: Vec::new(),
            author: default_author(),
            annot_style: AnnotStyle::default(),
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

fn now() -> u64 {
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

    pub fn add_recent(&mut self, path: &Path) {
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
