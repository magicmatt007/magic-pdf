//! User interface translations. Texts are written in English in the code and looked
//! up with [`t`]; texts with values use `{name}` placeholders and [`tf`].

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

mod de;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En = 0,
    De = 1,
}

static LANG: AtomicU8 = AtomicU8::new(0);

/// Languages for the settings: (setting value, name in that language).
pub const CHOICES: [(&str, &str); 3] = [("auto", "System"), ("en", "English"), ("de", "Deutsch")];

/// Sets the language from the setting ("auto", "en", "de").
pub fn set_language(setting: &str) {
    // Debug aid: MAGIC_PDF_LANG=de|en overrides the setting.
    let forced = std::env::var("MAGIC_PDF_LANG").ok();
    let lang = match forced.as_deref().unwrap_or(setting) {
        "de" => Lang::De,
        "en" => Lang::En,
        _ => detect(),
    };
    LANG.store(lang as u8, Ordering::Relaxed);
}

pub fn language() -> Lang {
    if LANG.load(Ordering::Relaxed) == Lang::De as u8 { Lang::De } else { Lang::En }
}

/// The system language, if magic-pdf speaks it.
fn detect() -> Lang {
    let locale = sys_locale::get_locale().unwrap_or_default().to_lowercase();
    if locale.starts_with("de") { Lang::De } else { Lang::En }
}

fn german() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| de::TEXTS.iter().copied().collect())
}

/// The text in the current language (the English text if there is no translation).
pub fn t(s: &'static str) -> &'static str {
    match language() {
        Lang::En => s,
        Lang::De => german().get(s).copied().unwrap_or(s),
    }
}

/// Translates `s` and fills in `{name}` placeholders.
pub fn tf(s: &'static str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
    let mut out = t(s).to_string();
    for (name, value) in args {
        out = out.replace(&format!("{{{name}}}"), &value.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn german_texts_keep_their_placeholders_and_shortcuts() {
        let placeholders = |s: &str| {
            let mut v: Vec<String> = s.split('{').skip(1).filter_map(|p| p.split_once('}')).map(|(n, _)| n.to_string()).collect();
            v.sort();
            v
        };
        for (en, de) in de::TEXTS {
            assert_eq!(placeholders(en), placeholders(de), "{en:?} → {de:?}");
            assert_eq!(en.contains('\t'), de.contains('\t'), "shortcut column: {en:?}");
            if let Some((_, key)) = en.split_once('\t') {
                assert!(de.ends_with(key), "same shortcut: {en:?} → {de:?}");
            }
            assert!(!de.is_empty(), "{en:?}");
        }
        let mut seen = std::collections::HashSet::new();
        for (en, _) in de::TEXTS {
            assert!(seen.insert(en), "duplicate entry {en:?}");
        }
    }

    /// Every text the code passes to `t`/`tf` has a German translation.
    #[test]
    fn every_ui_text_is_translated() {
        fn scan(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    scan(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") && !p.to_string_lossy().contains("i18n") {
                    let src = std::fs::read_to_string(&p).unwrap();
                    let b = src.as_bytes();
                    for (i, _) in src.match_indices("t(\"") {
                        let start = if i > 0 && b[i - 1] == b't' { i - 1 } else { i };
                        if start > 0 && (b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'_') {
                            continue;
                        }
                        let rest = &src[i + 3..];
                        let mut end = 0;
                        let mut escaped = false;
                        for (j, c) in rest.char_indices() {
                            if c == '"' && !escaped {
                                end = j;
                                break;
                            }
                            escaped = c == '\\' && !escaped;
                        }
                        let text = rest[..end].replace("\\t", "\t").replace("\\n", "\n").replace("\\\"", "\"");
                        out.push((text, p.display().to_string()));
                    }
                }
            }
        }
        let mut found = Vec::new();
        scan(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut found);
        assert!(found.len() > 200, "found {}", found.len());
        let german: std::collections::HashSet<&str> = de::TEXTS.iter().map(|(en, _)| *en).collect();
        let missing: Vec<_> = found.iter().filter(|(s, _)| !german.contains(s.as_str())).collect();
        assert!(missing.is_empty(), "texts without German translation: {missing:#?}");
    }

    #[test]
    fn lookup_and_placeholders() {
        set_language("de");
        assert_eq!(t("Open…\tCtrl+O"), "Öffnen…\tCtrl+O");
        assert_eq!(t("not translated"), "not translated");
        assert_eq!(tf("Page {page} of {pages}", &[("page", &3), ("pages", &10)]), "Seite 3 von 10");
        set_language("en");
        assert_eq!(tf("Page {page} of {pages}", &[("page", &3), ("pages", &10)]), "Page 3 of 10");
    }
}
