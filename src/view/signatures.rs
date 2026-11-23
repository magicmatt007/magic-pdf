//! The bar above signed documents and the signature details window.

use eframe::egui::{self, Color32, RichText, Ui};

use crate::engine::SignatureInfo;
use crate::engine::signatures::SigStatus;
use crate::i18n::{t, tf};
use crate::view::document_view::format_pdf_date;

const GREEN: Color32 = Color32::from_rgb(40, 160, 80);
const ORANGE: Color32 = Color32::from_rgb(210, 120, 0);
const RED: Color32 = Color32::from_rgb(210, 50, 50);

/// Icon, colour and short text for a signature's status.
fn status(s: &SignatureInfo) -> (&'static str, Color32, String) {
    match &s.check.status {
        SigStatus::Valid => ("✔", GREEN, t("Signature valid: the document is unchanged since it was signed.").into()),
        SigStatus::ChangedAfter => ("⚠", ORANGE, t("The signed version is unchanged, but the file was changed after signing.").into()),
        SigStatus::Invalid(why) => ("✖", RED, tf("Signature invalid: {why}.", &[("why", &t(why))])),
        SigStatus::Unknown(why) => ("?", ORANGE, tf("Signature not checked: {why}.", &[("why", &t(why))])),
    }
}

/// The worst status among the signatures, for the bar.
fn overall(sigs: &[SignatureInfo]) -> Option<&SignatureInfo> {
    let rank = |s: &SignatureInfo| match s.check.status {
        SigStatus::Invalid(_) => 0,
        SigStatus::Unknown(_) => 1,
        SigStatus::ChangedAfter => 2,
        SigStatus::Valid => 3,
    };
    sigs.iter().min_by_key(|s| rank(s))
}

/// The bar above a signed document. Returns true when the details are asked for.
pub fn bar(ui: &mut Ui, sigs: &[SignatureInfo]) -> bool {
    let Some(worst) = overall(sigs) else { return false };
    let (icon, color, _) = status(worst);
    let mut open = false;
    ui.horizontal(|ui| {
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
                    row(t("Signed on"), format_pdf_date(&s.time));
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
                if s.check.self_signed {
                    ui.colored_label(ORANGE, t("The certificate was issued by the signer itself, so anyone could have made one with this name."));
                }
            }
            ui.add_space(6.0);
            ui.weak(t("magic-pdf checks that the signed content is unchanged and that the signature matches the certificate in the file. It does not check whether that certificate is trusted or revoked."));
            ui.weak(t("Saving changes with magic-pdf rewrites the file, which invalidates these signatures."));
        });
    open
}
