//! Document properties, attached files and e-invoices (ZUGFeRD / Factur-X).

use std::path::PathBuf;

use eframe::egui::{self, Color32, RichText, Ui};

use crate::document::DocTab;
use crate::einvoice::{self, Invoice, money};
use crate::engine::{EditableProperties, PageOp, Request};
use crate::i18n::{t, tf};
use crate::view::document_view::format_pdf_date;

/// What to do with an attached file once the engine has read it.
#[derive(Clone, Debug, PartialEq)]
pub enum AttachmentAction {
    Open,
    Save(PathBuf),
    /// Read it as e-invoice data (done for XML attachments when a file opens).
    Inspect,
}

/// The e-invoice data found in a document.
pub struct EInvoice {
    pub name: String,
    pub xml: Vec<u8>,
    pub invoice: Result<Invoice, String>,
}

impl EInvoice {
    pub fn new(name: String, xml: Vec<u8>) -> Self {
        let invoice = einvoice::parse(&xml);
        EInvoice { name, xml, invoice }
    }

    /// "Factur-X / ZUGFeRD (EXTENDED)".
    pub fn label(&self) -> String {
        match &self.invoice {
            Ok(i) if i.profile_name == "XRechnung" => "XRechnung (CII)".into(),
            Ok(i) => format!("ZUGFeRD / Factur-X ({})", i.profile_name),
            Err(_) => "ZUGFeRD / Factur-X".into(),
        }
    }
}

fn file_size(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / 1_048_576.0)
    }
}

/// Lists the files attached to the document. Returns an action for an attachment.
pub fn attachments_list(ui: &mut Ui, tab: &DocTab, invoice: Option<&EInvoice>) -> Option<(usize, AttachmentAction)> {
    let info = tab.info()?;
    let mut action = None;
    if info.attachments.is_empty() {
        ui.weak(t("This document has no attached files."));
        return None;
    }
    if let Some(inv) = invoice {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(t("E-invoice")).strong());
            ui.weak(inv.label());
        });
        ui.separator();
    }
    for (i, a) in info.attachments.iter().enumerate() {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(&a.name).strong());
            ui.weak(file_size(a.size as u64));
            ui.horizontal(|ui| {
                if ui.button(t("Open")).on_hover_text(t("Open with the program your computer uses for this kind of file")).clicked() {
                    action = Some((i, AttachmentAction::Open));
                }
                if ui.button(t("Save…")).clicked() {
                    let mut dialog = rfd::FileDialog::new().set_file_name(&a.name);
                    if let Some(dir) = tab.path.parent() {
                        dialog = dialog.set_directory(dir);
                    }
                    if let Some(path) = dialog.save_file() {
                        action = Some((i, AttachmentAction::Save(path)));
                    }
                }
            });
        });
    }
    action
}

pub struct PropertiesDialog {
    pub doc: crate::engine::DocId,
    fields: EditableProperties,
}

impl PropertiesDialog {
    pub fn new(tab: &DocTab) -> Option<Self> {
        let p = &tab.info()?.properties;
        Some(PropertiesDialog {
            doc: tab.id,
            fields: EditableProperties {
                title: p.title.clone(),
                author: p.author.clone(),
                subject: p.subject.clone(),
                keywords: p.keywords.clone(),
            },
        })
    }
}

/// The document properties dialog. Returns (request to send, stays open).
pub fn properties_dialog(ctx: &egui::Context, d: &mut PropertiesDialog, tab: &DocTab) -> (Option<Request>, bool) {
    let Some(info) = tab.info() else { return (None, false) };
    let p = &info.properties;
    let mut result = (None, true);
    let mut open = true;
    egui::Window::new(t("Document properties"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            egui::Grid::new("props").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                for (label, value) in [
                    (t("Title"), &mut d.fields.title),
                    (t("Author"), &mut d.fields.author),
                    (t("Subject"), &mut d.fields.subject),
                    (t("Keywords"), &mut d.fields.keywords),
                ] {
                    ui.label(label);
                    ui.add(egui::TextEdit::singleline(value).desired_width(320.0));
                    ui.end_row();
                }
                ui.label("");
                ui.separator();
                ui.end_row();
                let size = std::fs::metadata(&tab.path).map(|m| file_size(m.len())).unwrap_or_default();
                let page = info.page_sizes.get(tab.current_page).map(|(w, h)| {
                    format!("{:.0} × {:.0} mm", w * 25.4 / 72.0, h * 25.4 / 72.0)
                });
                let rows = [
                    (t("Created with"), p.creator.clone()),
                    (t("PDF producer"), p.producer.clone()),
                    (t("Created"), format_pdf_date(&p.created)),
                    (t("Modified"), format_pdf_date(&p.modified)),
                    (t("PDF version"), p.version.map(|v| format!("{}.{}", v / 10, v % 10)).unwrap_or_default()),
                    (t("Pages"), info.page_sizes.len().to_string()),
                    (t("Page size"), page.unwrap_or_default()),
                    (t("File size"), size),
                    (t("File"), tab.path.display().to_string()),
                ];
                for (label, value) in rows {
                    if value.is_empty() {
                        continue;
                    }
                    ui.label(label);
                    ui.add(egui::Label::new(RichText::new(value).weak()).wrap());
                    ui.end_row();
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let changed = d.fields
                    != EditableProperties {
                        title: p.title.clone(),
                        author: p.author.clone(),
                        subject: p.subject.clone(),
                        keywords: p.keywords.clone(),
                    };
                if ui.add_enabled(changed, egui::Button::new(t("Apply"))).clicked() {
                    result = (
                        Some(Request::EditPages {
                            doc: d.doc,
                            op: PageOp::SetProperties {
                                properties: d.fields.clone(),
                            },
                        }),
                        false,
                    );
                }
                if ui.button(t("Close")).clicked() {
                    result.1 = false;
                }
            });
        });
    if !open {
        result.1 = false;
    }
    result
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(title).strong().size(15.0));
    ui.separator();
}

fn party_ui(ui: &mut Ui, title: &str, p: &einvoice::Party) {
    ui.vertical(|ui| {
        ui.label(RichText::new(title).weak());
        ui.label(RichText::new(&p.name).strong());
        for l in &p.address {
            ui.label(l);
        }
        if !p.vat_id.is_empty() {
            ui.label(format!("{} {}", t("VAT ID"), p.vat_id));
        }
        if !p.tax_number.is_empty() {
            ui.label(format!("{} {}", t("Tax number"), p.tax_number));
        }
        for c in &p.contact {
            ui.weak(c);
        }
    });
}

fn opt_money(v: Option<f64>) -> String {
    v.map(money).unwrap_or_default()
}

fn qty(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The e-invoice window. `page_text` is the document's text (to find the total on the
/// page), None while it is still being read. Returns false when closed; sets `save_xml`
/// when the user wants the XML saved.
pub fn einvoice_window(ctx: &egui::Context, inv: &EInvoice, page_text: Option<&str>, save_xml: &mut bool) -> bool {
    let mut open = true;
    egui::Window::new(t("E-invoice"))
        .open(&mut open)
        .default_size([760.0, 640.0])
        .resizable(true)
        .show(ctx, |ui| {
            let invoice = match &inv.invoice {
                Ok(i) => i,
                Err(e) => {
                    ui.colored_label(ui.visuals().error_fg_color, tf("The invoice data could not be read: {e}", &[("e", e)]));
                    *save_xml = ui.button(t("Save XML…")).clicked();
                    return;
                }
            };
            ui.horizontal_wrapped(|ui| {
                ui.heading(format!("{} {}", t(einvoice::type_name(&invoice.type_code)), invoice.number));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    *save_xml = ui.button(t("Save XML…")).clicked();
                    ui.weak(format!("{} · {}", inv.label(), inv.name));
                });
            });
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                invoice_body(ui, invoice, page_text);
            });
        });
    open
}

fn invoice_body(ui: &mut Ui, inv: &Invoice, page_text: Option<&str>) {
    let cur = &inv.currency;
    // Checks first: they say whether the rest can be trusted.
    let mut checks = einvoice::checks(inv);
    if let (Some(text), Some(total)) = (page_text, inv.totals.due.or(inv.totals.grand)) {
        let found = einvoice::amount_in_text(total, text);
        checks.push(einvoice::Check {
            ok: found,
            what: if found {
                tf("The amount {amount} also appears on the page", &[("amount", &money(total))])
            } else {
                tf("The amount {amount} was not found on the page; compare the page with this data", &[("amount", &money(total))])
            },
        });
    }
    for what in einvoice::missing_required(inv) {
        checks.push(einvoice::Check {
            ok: false,
            what: tf("Missing: {field}, required by EN 16931; a validator would reject this invoice", &[("field", &t(what))]),
        });
    }
    let all_ok = checks.iter().all(|c| c.ok);
    // Not an error (EN 16931 makes payment instructions optional), but worth knowing.
    let no_payment = inv.payment_means.is_empty() && inv.iban.is_empty();
    egui::CollapsingHeader::new(RichText::new(if all_ok {
        t("✔ The amounts in the invoice data add up")
    } else {
        t("⚠ The invoice data has problems")
    })
    .color(if all_ok { Color32::from_rgb(30, 140, 70) } else { Color32::from_rgb(210, 90, 0) })
    .strong())
    .default_open(!all_ok)
    .show(ui, |ui| {
        for c in &checks {
            let (mark, color) = if c.ok { ("✔", Color32::from_rgb(30, 140, 70)) } else { ("⚠", Color32::from_rgb(210, 90, 0)) };
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(color, mark);
                ui.label(&c.what);
            });
        }
        if no_payment {
            ui.horizontal_wrapped(|ui| {
                ui.weak("ℹ");
                ui.weak(t("The invoice data has no payment details (bank account); the recipient has to take them from the page"));
            });
        }
        ui.weak(t("The invoice data (XML) is the binding part of an e-invoice."));
    });

    ui.add_space(6.0);
    ui.columns(2, |cols| {
        party_ui(&mut cols[0], t("From"), &inv.seller);
        party_ui(&mut cols[1], t("To"), &inv.buyer);
    });

    section(ui, t("Details"));
    egui::Grid::new("einv_details").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
        let mut row = |label: &str, value: String| {
            if !value.is_empty() {
                ui.label(label);
                ui.label(value);
                ui.end_row();
            }
        };
        row(t("Invoice date"), inv.issue_date.clone());
        row(t("Delivery date"), inv.delivery_date.clone());
        if let Some((a, b)) = &inv.period {
            row(t("Billing period"), format!("{a} – {b}"));
        }
        row(t("Buyer reference"), inv.buyer_reference.clone());
        row(t("Order"), inv.order_reference.clone());
        row(t("Currency"), cur.clone());
    });
    for n in &inv.notes {
        ui.weak(n);
    }

    section(ui, &tf("Items ({n})", &[("n", &inv.lines.len())]));
    egui::Grid::new("einv_lines").striped(true).num_columns(6).spacing([12.0, 4.0]).show(ui, |ui| {
        for h in ["#", t("Item"), t("Quantity"), t("Price"), t("VAT"), t("Amount")] {
            ui.label(RichText::new(h).weak());
        }
        ui.end_row();
        for l in &inv.lines {
            ui.label(&l.id);
            ui.vertical(|ui| {
                ui.set_min_width(260.0);
                ui.set_max_width(360.0);
                ui.label(RichText::new(&l.name).strong());
                if !l.description.is_empty() && l.description != l.name {
                    ui.add(egui::Label::new(RichText::new(&l.description).weak()).wrap());
                }
                let mut extra = Vec::new();
                if !l.article.is_empty() {
                    extra.push(format!("{} {}", t("Art."), l.article));
                }
                if !l.delivery_date.is_empty() {
                    extra.push(format!("{} {}", t("delivered"), l.delivery_date));
                }
                if !extra.is_empty() {
                    ui.weak(extra.join(" · "));
                }
            });
            ui.label(l.quantity.map(|q| format!("{} {}", qty(q), l.unit)).unwrap_or_default());
            ui.label(match (l.net_price, l.price_basis.filter(|b| (*b - 1.0).abs() > 1e-9)) {
                (Some(p), Some(b)) => format!("{} / {} {}", money(p), qty(b), l.unit),
                (Some(p), None) => money(p),
                _ => String::new(),
            });
            ui.label(l.vat_rate.map(|r| format!("{}%", qty(r))).unwrap_or_default());
            ui.label(RichText::new(opt_money(l.total)).strong());
            ui.end_row();
        }
    });

    section(ui, t("VAT"));
    egui::Grid::new("einv_tax").striped(true).num_columns(4).spacing([16.0, 4.0]).show(ui, |ui| {
        for h in [t("Rate"), t("Category"), t("Taxable amount"), t("VAT")] {
            ui.label(RichText::new(h).weak());
        }
        ui.end_row();
        for x in &inv.taxes {
            ui.label(x.rate.map(|r| format!("{}%", qty(r))).unwrap_or_default());
            let cat = einvoice::tax_category_name(&x.category);
            ui.label(if cat.is_empty() { x.category.clone() } else { t(cat).to_string() });
            ui.label(opt_money(x.basis));
            ui.label(opt_money(x.amount));
            ui.end_row();
        }
    });

    section(ui, t("Totals"));
    egui::Grid::new("einv_totals").num_columns(2).spacing([24.0, 4.0]).show(ui, |ui| {
        let tt = &inv.totals;
        let mut row = |label: &str, v: Option<f64>, strong: bool| {
            if let Some(v) = v {
                ui.label(label);
                let text = RichText::new(format!("{} {cur}", money(v)));
                ui.label(if strong { text.strong().size(15.0) } else { text });
                ui.end_row();
            }
        };
        row(t("Line total"), tt.lines, false);
        row(t("Charges"), tt.charges, false);
        row(t("Allowances"), tt.allowances, false);
        row(t("Taxable amount"), tt.tax_basis, false);
        row(t("VAT"), tt.tax, false);
        row(t("Total"), tt.grand, true);
        row(t("Prepaid"), tt.prepaid, false);
        row(t("Amount due"), tt.due, true);
    });

    section(ui, t("Payment"));
    egui::Grid::new("einv_pay").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
        let mut row = |label: &str, value: String| {
            if !value.is_empty() {
                ui.label(label);
                ui.label(value);
                ui.end_row();
            }
        };
        row(t("Due date"), inv.due_date.clone());
        for terms in &inv.payment_terms {
            row(t("Terms"), terms.clone());
        }
        row(t("IBAN"), inv.iban.join(", "));
        row(t("BIC"), inv.bic.clone());
        row(t("Account holder"), inv.account_name.clone());
        row(t("Payment reference"), inv.payment_reference.clone());
    });
    if inv.iban.is_empty() {
        ui.weak(t("The invoice data contains no bank account."));
    }
}
