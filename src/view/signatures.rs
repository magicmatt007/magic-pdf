//! The bar above signed documents and the signature details window.

use eframe::egui::{self, Color32, RichText, Ui};

use crate::engine::SignatureInfo;
use crate::engine::signatures::SigStatus;
use crate::engine::trust::Trust;
use crate::i18n::{t, tf};
use crate::view::document_view::format_pdf_date;

const GREEN: Color32 = Color32::from_rgb(40, 160, 80);
const ORANGE: Color32 = Color32::from_rgb(210, 120, 0);
const RED: Color32 = Color32::from_rgb(210, 50, 50);

/// Icon, colour and short text for a signature's status.
fn status(s: &SignatureInfo) -> (&'static str, Color32, String) {
    match &s.check.status {
        SigStatus::Valid => ("✔", GREEN, t("Signature valid: the document is unchanged since it was signed.").into()),
        SigStatus::ChangedAfter => {
            ("⚠", ORANGE, t("The signed version is unchanged, but the file was changed after signing.").into())
        }
        SigStatus::Invalid(why) => ("✖", RED, tf("Signature invalid: {why}.", &[("why", &t(why))])),
        SigStatus::Unknown(why) => ("?", ORANGE, tf("Signature not checked: {why}.", &[("why", &t(why))])),
    }
}

/// Whether the signer's certificate is trusted, in words, and its colour.
fn trust_text(s: &SignatureInfo) -> (String, Color32) {
    let (text, color) = match &s.trust {
        None => (t("Checking the certificate…"), ORANGE),
        Some(Trust::Trusted { revocation_checked: true }) => {
            (t("Certificate trusted: issued by a trusted authority and not revoked."), GREEN)
        }
        Some(Trust::Trusted { revocation_checked: false }) => {
            (t("Certificate trusted: issued by a trusted authority (revocation not checked)."), GREEN)
        }
        Some(Trust::UntrustedRoot) => {
            (t("Certificate not trusted: it is not from an authority this computer trusts."), ORANGE)
        }
        Some(Trust::IssuerUnknown) => (t("Certificate not trusted: its issuer is unknown."), ORANGE),
        Some(Trust::Expired) => (t("Certificate not valid at the time of signing (expired or not yet valid)."), RED),
        Some(Trust::Revoked) => (t("Certificate revoked by its issuer."), RED),
        Some(Trust::Unknown) => (t("The certificate could not be checked."), ORANGE),
    };
    (text.to_string(), color)
}

fn trusted(s: &SignatureInfo) -> bool {
    matches!(s.trust, Some(Trust::Trusted { .. }))
}

/// The worst status among the signatures, for the bar.
fn overall(sigs: &[SignatureInfo]) -> Option<&SignatureInfo> {
    let rank = |s: &SignatureInfo| match (&s.check.status, &s.trust) {
        (SigStatus::Invalid(_), _) | (_, Some(Trust::Revoked | Trust::Expired)) => 0,
        (SigStatus::Unknown(_), _) => 1,
        (_, _) if !trusted(s) => 2,
        (SigStatus::ChangedAfter, _) => 3,
        (SigStatus::Valid, _) => 4,
    };
    sigs.iter().min_by_key(|s| rank(s))
}

/// The bar above a signed document. Returns true when the details are asked for.
pub fn bar(ui: &mut Ui, sigs: &[SignatureInfo]) -> bool {
    let Some(worst) = overall(sigs) else { return false };
    let (icon, color, _) = status(worst);
    // The worse of the content check and the certificate check decides the icon.
    let (icon, color) = match (&worst.check.status, &worst.trust) {
        (SigStatus::Valid | SigStatus::ChangedAfter, Some(Trust::Revoked | Trust::Expired)) => ("✖", RED),
        (SigStatus::Valid, _) if !trusted(worst) => ("⚠", ORANGE),
        _ => (icon, color),
    };
    let mut open = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(t("Signed")).strong());
        ui.colored_label(color, icon);
        let who: Vec<&str> = sigs.iter().map(|s| s.check.signer.as_str()).filter(|n| !n.is_empty()).collect();
        let text = match (&worst.check.status, sigs.len()) {
            (SigStatus::Valid, 1) => tf("Signed by {name}; unchanged since signing.", &[("name", &who.join(", "))]),
            (SigStatus::Valid, n) => tf("{n} signatures, all valid.", &[("n", &n)]),
            (SigStatus::ChangedAfter, _) => t("Changed after signing.").into(),
            (SigStatus::Invalid(_), _) => t("A signature is invalid.").into(),
            (SigStatus::Unknown(_), _) => t("A signature could not be checked.").into(),
        };
        ui.label(text);
        if matches!(worst.check.status, SigStatus::Valid | SigStatus::ChangedAfter) {
            let (trust, trust_color) = trust_text(worst);
            ui.colored_label(trust_color, trust);
        }
        if ui.button(t("Signature details")).clicked() {
            open = true;
        }
    });
    open
}

/// The details window; false when closed.
pub fn window(ctx: &egui::Context, sigs: &[SignatureInfo]) -> bool {
    let mut open = true;
    egui::Window::new(t("Signatures"))
        .id(egui::Id::unique("signature_details"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(460.0)
        .show(ctx, |ui| {
            for (i, s) in sigs.iter().enumerate() {
                if i > 0 {
                    ui.separator();
                }
                let (icon, color, text) = status(s);
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(color, RichText::new(icon).strong());
                    ui.label(text);
                });
                egui::Grid::new(("sig", i)).num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                    let mut row = |k: &str, v: String| {
                        if !v.is_empty() {
                            ui.weak(k);
                            ui.label(v);
                            ui.end_row();
                        }
                    };
                    row(t("Signed by"), s.check.signer.clone());
                    row(t("Certificate issued by"), s.check.issuer.clone());
                    row(t("Certificate"), trust_text(s).0);
                    row(t("Signed on"), format_pdf_date(&s.time));
                    if let Some(ts) = &s.check.timestamp {
                        let when = tf(
                            "{time} UTC by {authority}",
                            &[("time", &format_pdf_date(&ts.time)), ("authority", &ts.authority)],
                        );
                        let note = match (ts.valid, s.timestamp_trusted) {
                            (false, _) => t("does not match this signature"),
                            (true, Some(true)) => t("trusted"),
                            (true, Some(false)) => t("authority not trusted"),
                            (true, None) => t("checking…"),
                        };
                        row(t("Timestamp"), format!("{when} ({note})"));
                    }
                    row(t("Reason"), s.reason.clone());
                    row(
                        t("Allowed changes"),
                        match s.permission {
                            1 => t("none (certified)").into(),
                            2 => t("filling in forms and signing (certified)").into(),
                            3 => t("forms, signing and comments (certified)").into(),
                            _ => String::new(),
                        },
                    );
                });
                // Not needed when this computer trusts the certificate anyway.
                if s.check.self_signed && !trusted(s) {
                    ui.colored_label(ORANGE, t("The certificate was issued by the signer itself, so anyone could have made one with this name."));
                }
            }
            ui.add_space(6.0);
            ui.weak(if cfg!(windows) {
                t("Certificates are checked with Windows, against its trusted authorities and their revocation lists; with a trusted timestamp, at the time of signing.")
            } else {
                t("Certificates are checked against this computer's trusted authorities (revocation is not checked); with a trusted timestamp, at the time of signing.")
            });
            ui.weak(t("Comments and page changes saved with magic-pdf are added after the signed part, so the signatures stay intact. Only redaction rewrites the file."));
        });
    open
}
