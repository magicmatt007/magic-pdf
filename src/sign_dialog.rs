//! The "Sign with certificate" dialog: choosing the certificate file, its password,
//! the picture and the details, then signing in the background.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui::{self, Ui};

use crate::engine::sign::{IdentityError, SignRequest, SignatureMark, SigningIdentity};
use crate::engine::{DocId, PdfBox, StampSpec, pdf_date_now};
use crate::i18n::{t, tf};
use crate::settings::{Settings, Signature};
use crate::view::document_view::format_pdf_date;

pub struct SignDialog {
    pub doc: DocId,
    page: usize,
    rect: PdfBox,
    password: String,
    /// Why the certificate could not be opened.
    problem: Option<String>,
    focus_password: bool,
    /// Windows: the certificates in the Windows certificate store (listed once).
    #[cfg(windows)]
    store: Option<Result<Vec<crate::engine::cert_store::StoreCertificate>, String>>,
}

/// What the dialog asks the app to do.
pub enum SignAction {
    Keep,
    Close,
    /// Sign the file on disk and write the result to `out`.
    Sign {
        request: Box<SignRequest>,
        identity: Arc<SigningIdentity>,
        out: PathBuf,
    },
}

impl SignDialog {
    pub fn new(doc: DocId, page: usize, rect: PdfBox) -> SignDialog {
        SignDialog {
            doc,
            page,
            rect,
            password: String::new(),
            problem: None,
            focus_password: true,
            #[cfg(windows)]
            store: None,
        }
    }

    /// Shows the dialog. `identities` keeps opened certificates for this session, so
    /// the password is asked once.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        settings: &mut Settings,
        identities: &mut HashMap<PathBuf, Arc<SigningIdentity>>,
        file: &Path,
        unsaved: bool,
    ) -> SignAction {
        let mut action = SignAction::Keep;
        let mut open = true;
        egui::Window::new(t("Sign with certificate"))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(420.0);
                let identity = self.certificate_ui(ui, settings, identities);
                ui.add_space(6.0);
                egui::Grid::new("sign_details").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label(t("Picture"));
                    picture_choice(ui, settings);
                    ui.end_row();
                    ui.label(t("Reason"));
                    ui.add(
                        egui::TextEdit::singleline(&mut settings.sign_reason)
                            .hint_text(t("optional, e.g. Approved"))
                            .desired_width(260.0),
                    );
                    ui.end_row();
                    ui.label(t("Location"));
                    ui.add(
                        egui::TextEdit::singleline(&mut settings.sign_location)
                            .hint_text(t("optional"))
                            .desired_width(260.0),
                    );
                    ui.end_row();
                });
                ui.checkbox(&mut settings.sign_timestamp, t("Add a trusted timestamp (needs an internet connection)"))
                    .on_hover_text(tf(
                        "Proof of the signing time from {server}, so the signature can be checked even after the certificate expires",
                        &[("server", &settings.timestamp_server)],
                    ));
                if unsaved {
                    ui.add_space(4.0);
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        t("The document has unsaved changes. Save it first: the signature covers the saved file."),
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let ready = identity.is_some() && !unsaved;
                    let button = ui.add_enabled(ready, crate::theme::primary_button(t("Sign and save…")));
                    if button.clicked()
                        && let Some(identity) = identity
                        && let Some(out) = rfd::FileDialog::new()
                            .add_filter(t("PDF documents"), &["pdf"])
                            .set_directory(file.parent().unwrap_or(Path::new(".")))
                            .set_file_name(file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
                            .save_file()
                    {
                        action = SignAction::Sign { request: Box::new(self.request(settings, &identity)), identity, out };
                    }
                    if ui.button(t("Cancel")).clicked() {
                        action = SignAction::Close;
                    }
                });
            });
        if !open {
            action = SignAction::Close;
        }
        action
    }

    /// The certificate file and its password; returns the opened certificate.
    fn certificate_ui(
        &mut self,
        ui: &mut Ui,
        settings: &mut Settings,
        identities: &mut HashMap<PathBuf, Arc<SigningIdentity>>,
    ) -> Option<Arc<SigningIdentity>> {
        #[cfg(windows)]
        {
            ui.horizontal(|ui| {
                ui.radio_value(&mut settings.sign_from_store, true, t("Windows certificate store"));
                ui.radio_value(&mut settings.sign_from_store, false, t("Certificate file"));
            });
            if settings.sign_from_store {
                return self.store_ui(ui, settings, identities);
            }
        }
        ui.horizontal(|ui| {
            ui.label(t("Certificate"));
            match &settings.sign_certificate {
                Some(p) => ui.strong(p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
                None => ui.weak(t("none chosen")),
            }
            .on_hover_text(settings.sign_certificate.as_ref().map(|p| p.display().to_string()).unwrap_or_default());
            if ui.button(t("Choose…")).clicked()
                && let Some(p) = rfd::FileDialog::new().add_filter(t("Certificate files"), &["p12", "pfx"]).pick_file()
            {
                settings.sign_certificate = Some(p);
                self.password.clear();
                self.problem = None;
                self.focus_password = true;
            }
        });
        let path = settings.sign_certificate.clone()?;
        if let Some(id) = identities.get(&path) {
            signer_ui(ui, id);
            return Some(id.clone());
        }
        let mut submit = false;
        ui.horizontal(|ui| {
            ui.label(t("Password"));
            let resp = ui.add(egui::TextEdit::singleline(&mut self.password).password(true).desired_width(200.0));
            if std::mem::take(&mut self.focus_password) {
                resp.request_focus();
            }
            submit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            submit |= ui.button(t("Open")).clicked();
        });
        if submit {
            match std::fs::read(&path) {
                Err(e) => {
                    self.problem = Some(tf("Could not read {file}: {e}", &[("file", &path.display()), ("e", &e)]))
                }
                Ok(data) => match SigningIdentity::from_pkcs12(&data, &self.password) {
                    Ok(id) => {
                        self.password.clear();
                        self.problem = None;
                        identities.insert(path.clone(), Arc::new(id));
                    }
                    Err(e) => {
                        self.problem = Some(
                            match e {
                                IdentityError::Password => {
                                    t("Wrong password, or not a certificate file (.p12 or .pfx).")
                                }
                                IdentityError::NoKey => t("The file holds no certificate with a private key."),
                                IdentityError::UnsupportedKey => {
                                    t("This kind of key is not supported (RSA and ECDSA P-256/P-384 are).")
                                }
                            }
                            .to_string(),
                        );
                        self.focus_password = true;
                    }
                },
            }
        }
        if let Some(p) = &self.problem {
            ui.colored_label(ui.visuals().error_fg_color, p);
        }
        identities.get(&path).cloned()
    }

    /// Windows: choosing a certificate from the Windows certificate store.
    #[cfg(windows)]
    fn store_ui(
        &mut self,
        ui: &mut Ui,
        settings: &mut Settings,
        identities: &mut HashMap<PathBuf, Arc<SigningIdentity>>,
    ) -> Option<Arc<SigningIdentity>> {
        use crate::engine::cert_store;
        let list = match self.store.get_or_insert_with(cert_store::list) {
            Ok(list) => list.clone(),
            Err(e) => {
                ui.colored_label(ui.visuals().error_fg_color, e.as_str());
                return None;
            }
        };
        if list.is_empty() {
            ui.weak(t("There is no certificate with a private key in your Windows certificate store."));
            return None;
        }
        let label = |c: &cert_store::StoreCertificate| {
            tf(
                "{name} (issued by {issuer}, valid until {date})",
                &[("name", &c.name), ("issuer", &c.issuer), ("date", &c.expires)],
            )
        };
        let chosen = settings
            .sign_store_certificate
            .as_ref()
            .and_then(|t| list.iter().find(|c| &c.thumbprint == t))
            .unwrap_or(&list[0])
            .clone();
        egui::ComboBox::from_id_salt("store_certificate").width(400.0).selected_text(label(&chosen)).show_ui(
            ui,
            |ui| {
                for c in &list {
                    if ui.selectable_label(c.thumbprint == chosen.thumbprint, label(c)).clicked() {
                        settings.sign_store_certificate = Some(c.thumbprint.clone());
                    }
                }
            },
        );
        let key = PathBuf::from(format!("windows-store:{}", chosen.thumbprint));
        if !identities.contains_key(&key) {
            match cert_store::identity(&chosen.thumbprint) {
                Ok(id) => {
                    identities.insert(key.clone(), Arc::new(id));
                }
                Err(e) => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                    return None;
                }
            }
        }
        let id = identities.get(&key)?;
        signer_ui(ui, id);
        Some(id.clone())
    }

    fn request(&self, settings: &Settings, id: &SigningIdentity) -> SignRequest {
        let date = pdf_date_now();
        let mut lines = vec![
            tf("Digitally signed by {name}", &[("name", &id.name)]),
            tf("Date: {date}", &[("date", &format!("{} UTC", format_pdf_date(&date)))]),
        ];
        let (reason, location) = (settings.sign_reason.trim(), settings.sign_location.trim());
        if !reason.is_empty() {
            lines.push(tf("Reason: {reason}", &[("reason", &reason)]));
        }
        if !location.is_empty() {
            lines.push(tf("Location: {location}", &[("location", &location)]));
        }
        let r = self.rect;
        SignRequest {
            page: self.page,
            rect: [r.left, r.bottom, r.right, r.top],
            mark: mark(settings),
            lines,
            reason: reason.to_string(),
            location: location.to_string(),
            date,
            timestamp_url: settings.sign_timestamp.then(|| settings.timestamp_server.clone()),
        }
    }
}

/// Who the certificate names, and a warning for a self-made one.
fn signer_ui(ui: &mut Ui, id: &SigningIdentity) {
    ui.label(tf("Signer: {name}", &[("name", &id.name)]));
    if !id.expires.is_empty() {
        ui.weak(tf("Valid until {date}", &[("date", &id.expires)]));
    }
    if id.self_signed {
        ui.weak(t("A self-made certificate: others can check that the document is unchanged, but not who signed it."));
    } else {
        ui.weak(tf("Certificate issued by {issuer}", &[("issuer", &id.issuer)]));
    }
}

fn picture_choice(ui: &mut Ui, settings: &mut Settings) {
    let label = |i: Option<usize>| match i {
        Some(i) => tf("Signature {n}", &[("n", &(i + 1))]),
        None => t("None (text only)").to_string(),
    };
    let current = settings.sign_picture.filter(|&i| i < settings.signatures.len());
    egui::ComboBox::from_id_salt("sign_picture").selected_text(label(current)).show_ui(ui, |ui| {
        if ui.selectable_label(current.is_none(), label(None)).clicked() {
            settings.sign_picture = None;
        }
        for i in 0..settings.signatures.len() {
            if ui.selectable_label(current == Some(i), label(Some(i))).clicked() {
                settings.sign_picture = Some(i);
            }
        }
    });
}

/// The chosen signature picture.
fn mark(settings: &Settings) -> SignatureMark {
    match settings.sign_picture.and_then(|i| settings.signatures.get(i)) {
        Some(Signature::Drawn(strokes)) => {
            SignatureMark::Drawn { strokes: strokes.clone(), color: settings.annot_style.signature_color }
        }
        Some(Signature::Image(path)) => match crate::view::stamps::load_image(path) {
            Ok(StampSpec::Image { width, height, rgba }) => SignatureMark::Image { width, height, rgba },
            _ => SignatureMark::None,
        },
        None => SignatureMark::None,
    }
}

/// Signs `file` and writes the result to `out` (run on a background thread).
pub fn sign_file(file: &Path, out: &Path, request: &SignRequest, identity: &SigningIdentity) -> Result<(), String> {
    let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
    let signed = crate::engine::sign::sign(&bytes, request, identity, &fetch_timestamp)?;
    crate::engine::write_atomically(out, &signed).map_err(|e| e.to_string())
}

/// Sends an RFC 3161 request to a timestamp server.
fn fetch_timestamp(url: &str, request: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = crate::update::curl()
        .args(["--max-time", "60", "-H", "Content-Type: application/timestamp-query", "--data-binary", "@-"])
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run curl: {e}"))?;
    child.stdin.take().ok_or("could not run curl")?.write_all(request).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(tf(
            "the timestamp server {server} could not be reached: {e}",
            &[("server", &url), ("e", &String::from_utf8_lossy(&out.stderr).trim())],
        ));
    }
    Ok(out.stdout)
}
