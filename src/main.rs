#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod document;
mod engine;
mod fonts;
mod einvoice;
mod i18n;
mod icons;
mod ipc;
mod keys;
mod ocr_dialog;
#[cfg(target_os = "linux")]
mod pinch;
#[cfg(target_os = "macos")]
mod macos;
mod print;
mod settings;
mod theme;
mod update;
mod view;

use std::ffi::OsString;
use std::path::PathBuf;

/// Command line: `magic-pdf [--new-window] [--at X,Y] [--take FILE ORIGINAL PAGE DIRTY] [FILE…]`.
/// `--take` and `--at` are used internally when a tab is dragged into a new window.
#[derive(Debug, Default, PartialEq)]
pub struct Args {
    pub files: Vec<PathBuf>,
    pub new_window: bool,
    pub at: Option<[f32; 2]>,
    pub take: Option<ipc::TabTransfer>,
}

pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Args {
    let mut out = Args::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--new-window") => out.new_window = true,
            Some("--at") => {
                out.at = it.next().and_then(|v| {
                    let (x, y) = v.to_str()?.split_once(',')?;
                    Some([x.parse().ok()?, y.parse().ok()?])
                })
            }
            Some("--take") => {
                let mut next = || it.next();
                if let (Some(file), Some(original), Some(page), Some(dirty)) = (next(), next(), next(), next()) {
                    out.take = Some(ipc::TabTransfer {
                        file: file.into(),
                        original: original.into(),
                        page: page.to_str().and_then(|p| p.parse().ok()).unwrap_or(0),
                        dirty: dirty.to_str() == Some("1"),
                    });
                    out.new_window = true;
                }
            }
            _ => out.files.push(PathBuf::from(a)),
        }
    }
    out
}

/// Hands the files to an already running magic-pdf. Returns true if it took them.
fn hand_to_running_instance(files: &[PathBuf]) -> bool {
    let files: Vec<PathBuf> = files
        .iter()
        .map(|f| std::fs::canonicalize(f).unwrap_or_else(|_| f.clone()))
        .collect();
    ipc::peers()
        .into_iter()
        .any(|peer| files.iter().all(|f| peer.send(&ipc::Message::Open(f.clone()))))
}

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = parse_args(std::env::args_os().skip(1));
    // Opening a PDF while magic-pdf runs adds a tab to that window instead.
    if !args.new_window && !args.files.is_empty() && hand_to_running_instance(&args.files) {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    macos::install();

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("magic-pdf")
        .with_app_id("magic-pdf")
        .with_inner_size([1600.0, 900.0])
        .with_min_inner_size([480.0, 320.0])
        .with_drag_and_drop(true)
        .with_icon(eframe::egui::IconData {
            rgba: include_bytes!("../assets/icons/magic-pdf-64.rgba").to_vec(),
            width: 64,
            height: 64,
        });
    if let Some([x, y]) = args.at {
        // A tab dropped outside its window: open the new window there.
        viewport = viewport.with_position([x - 120.0, y - 60.0]).with_inner_size([1200.0, 800.0]);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "magic-pdf",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, args)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn command_line() {
        let a = parse_args(os(&["a.pdf", "b.pdf"]));
        assert_eq!(a.files.len(), 2);
        assert!(!a.new_window);
        let a = parse_args(os(&["--at", "100,200.5", "--take", "/tmp/x.pdf", "/home/y.pdf", "3", "1"]));
        assert_eq!(a.at, Some([100.0, 200.5]));
        assert!(a.new_window);
        assert_eq!(
            a.take,
            Some(ipc::TabTransfer {
                file: "/tmp/x.pdf".into(),
                original: "/home/y.pdf".into(),
                page: 3,
                dirty: true,
            })
        );
        assert!(a.files.is_empty());
    }
}
