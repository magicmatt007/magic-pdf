//! E-invoices: the machine-readable XML inside ZUGFeRD / Factur-X PDFs (UN/CEFACT
//! Cross Industry Invoice). Reads what people look at and checks the arithmetic.

use roxmltree::{Document, Node};

use crate::i18n::{t, tf};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Party {
    pub name: String,
    /// Street lines, then "postcode city", then the country.
    pub address: Vec<String>,
    pub vat_id: String,
    pub tax_number: String,
    pub contact: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    pub id: String,
    pub name: String,
    pub description: String,
    pub article: String,
    pub quantity: Option<f64>,
    pub unit: String,
    pub net_price: Option<f64>,
    /// Quantity the net price is for (usually 1).
    pub price_basis: Option<f64>,
    pub vat_rate: Option<f64>,
    pub vat_category: String,
    pub total: Option<f64>,
    pub delivery_date: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tax {
    pub category: String,
    pub rate: Option<f64>,
    pub basis: Option<f64>,
    pub amount: Option<f64>,
    pub exemption: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Totals {
    pub lines: Option<f64>,
    pub charges: Option<f64>,
    pub allowances: Option<f64>,
    pub tax_basis: Option<f64>,
    pub tax: Option<f64>,
    pub grand: Option<f64>,
    pub prepaid: Option<f64>,
    pub rounding: Option<f64>,
    pub due: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Invoice {
    /// The profile URN and its short name (e.g. "EXTENDED").
    pub profile: String,
    pub profile_name: String,
    pub number: String,
    pub type_code: String,
    pub issue_date: String,
    pub currency: String,
    pub notes: Vec<String>,
    pub seller: Party,
    pub buyer: Party,
    pub buyer_reference: String,
    pub order_reference: String,
    pub delivery_date: String,
    pub period: Option<(String, String)>,
    pub lines: Vec<Line>,
    pub taxes: Vec<Tax>,
    pub totals: Totals,
    pub due_date: String,
    pub payment_terms: Vec<String>,
    pub iban: Vec<String>,
    /// Payment means codes (UNTDID 4461, e.g. 58 SEPA transfer).
    pub payment_means: Vec<String>,
    pub bic: String,
    pub account_name: String,
    pub payment_reference: String,
}

/// Whether `xml` looks like an e-invoice this module reads.
pub fn is_invoice_xml(xml: &[u8]) -> bool {
    let head = &xml[..xml.len().min(4000)];
    let s = String::from_utf8_lossy(head);
    s.contains("CrossIndustryInvoice")
}

fn child<'a>(n: Node<'a, 'a>, name: &str) -> Option<Node<'a, 'a>> {
    n.children().find(|c| c.is_element() && c.tag_name().name() == name)
}

fn children<'a>(n: Node<'a, 'a>, name: &'a str) -> impl Iterator<Item = Node<'a, 'a>> + 'a {
    n.children().filter(move |c| c.is_element() && c.tag_name().name() == name)
}

/// The node at a path of element names below `n`.
fn at<'a>(n: Node<'a, 'a>, path: &[&str]) -> Option<Node<'a, 'a>> {
    path.iter().try_fold(n, |n, p| child(n, p))
}

fn text(n: Option<Node>) -> String {
    n.and_then(|n| n.text()).map(|t| t.trim().to_string()).unwrap_or_default()
}

fn txt(n: Node, path: &[&str]) -> String {
    text(at(n, path))
}

fn num(n: Node, path: &[&str]) -> Option<f64> {
    txt(n, path).parse().ok()
}

/// A CII date (format 102: YYYYMMDD) as YYYY-MM-DD.
fn date(n: Node, path: &[&str]) -> String {
    let Some(d) = at(n, path) else { return String::new() };
    let s = text(d.descendants().find(|c| c.tag_name().name() == "DateTimeString").or(Some(d)));
    if s.len() == 8 && s.chars().all(|c| c.is_ascii_digit()) {
        format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8])
    } else {
        s
    }
}

fn party(n: Option<Node>) -> Party {
    let Some(n) = n else { return Party::default() };
    let mut p = Party {
        name: txt(n, &["Name"]),
        ..Default::default()
    };
    if let Some(a) = child(n, "PostalTradeAddress") {
        for l in ["LineOne", "LineTwo", "LineThree"] {
            let v = txt(a, &[l]);
            if !v.is_empty() {
                p.address.push(v);
            }
        }
        let city = format!("{} {}", txt(a, &["PostcodeCode"]), txt(a, &["CityName"])).trim().to_string();
        if !city.is_empty() {
            p.address.push(city);
        }
        let region = txt(a, &["CountrySubDivisionName"]);
        let country = txt(a, &["CountryID"]);
        let last = [region, country].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
        if !last.is_empty() {
            p.address.push(last);
        }
    }
    for reg in children(n, "SpecifiedTaxRegistration") {
        if let Some(id) = child(reg, "ID") {
            let v = text(Some(id));
            match id.attribute("schemeID") {
                Some("VA") => p.vat_id = v,
                _ => p.tax_number = v,
            }
        }
    }
    if let Some(c) = child(n, "DefinedTradeContact") {
        for v in [
            txt(c, &["PersonName"]),
            txt(c, &["DepartmentName"]),
            txt(c, &["TelephoneUniversalCommunication", "CompleteNumber"]),
            txt(c, &["EmailURIUniversalCommunication", "URIID"]),
        ] {
            if !v.is_empty() {
                p.contact.push(v);
            }
        }
    }
    let email = txt(n, &["URIUniversalCommunication", "URIID"]);
    if !email.is_empty() && !p.contact.contains(&email) {
        p.contact.push(email);
    }
    p
}

pub fn parse(xml: &[u8]) -> Result<Invoice, String> {
    let s = std::str::from_utf8(xml).map_err(|_| "the invoice data is not UTF-8 text".to_string())?;
    let doc = Document::parse(s).map_err(|e| format!("the invoice data is not valid XML: {e}"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "CrossIndustryInvoice" {
        return Err(format!("unknown invoice format <{}>", root.tag_name().name()));
    }
    let mut inv = Invoice {
        profile: txt(root, &["ExchangedDocumentContext", "GuidelineSpecifiedDocumentContextParameter", "ID"]),
        ..Default::default()
    };
    inv.profile_name = profile_name(&inv.profile).to_string();
    if let Some(d) = child(root, "ExchangedDocument") {
        inv.number = txt(d, &["ID"]);
        inv.type_code = txt(d, &["TypeCode"]);
        inv.issue_date = date(d, &["IssueDateTime"]);
        inv.notes = children(d, "IncludedNote").map(|n| txt(n, &["Content"])).filter(|s| !s.is_empty()).collect();
    }
    let Some(tx) = child(root, "SupplyChainTradeTransaction") else { return Ok(inv) };
    for item in children(tx, "IncludedSupplyChainTradeLineItem") {
        let product = child(item, "SpecifiedTradeProduct");
        let price = at(item, &["SpecifiedLineTradeAgreement", "NetPriceProductTradePrice"]);
        let delivery = child(item, "SpecifiedLineTradeDelivery");
        let settle = child(item, "SpecifiedLineTradeSettlement");
        let qty = delivery.and_then(|d| child(d, "BilledQuantity"));
        inv.lines.push(Line {
            id: txt(item, &["AssociatedDocumentLineDocument", "LineID"]),
            name: product.map(|p| txt(p, &["Name"])).unwrap_or_default(),
            description: product.map(|p| txt(p, &["Description"])).unwrap_or_default(),
            article: product
                .map(|p| {
                    let s = txt(p, &["SellerAssignedID"]);
                    if s.is_empty() { txt(p, &["GlobalID"]) } else { s }
                })
                .unwrap_or_default(),
            quantity: text(qty).parse().ok(),
            unit: qty.and_then(|q| q.attribute("unitCode")).map(unit_name).unwrap_or_default(),
            net_price: price.and_then(|p| num(p, &["ChargeAmount"])),
            price_basis: price.and_then(|p| num(p, &["BasisQuantity"])),
            vat_rate: settle.and_then(|s| num(s, &["ApplicableTradeTax", "RateApplicablePercent"])),
            vat_category: settle.map(|s| txt(s, &["ApplicableTradeTax", "CategoryCode"])).unwrap_or_default(),
            total: settle.and_then(|s| num(s, &["SpecifiedTradeSettlementLineMonetarySummation", "LineTotalAmount"])),
            delivery_date: delivery.map(|d| date(d, &["ActualDeliverySupplyChainEvent", "OccurrenceDateTime"])).unwrap_or_default(),
        });
    }
    if let Some(a) = child(tx, "ApplicableHeaderTradeAgreement") {
        inv.seller = party(child(a, "SellerTradeParty"));
        inv.buyer = party(child(a, "BuyerTradeParty"));
        inv.buyer_reference = txt(a, &["BuyerReference"]);
        inv.order_reference = txt(a, &["BuyerOrderReferencedDocument", "IssuerAssignedID"]);
    }
    if let Some(d) = child(tx, "ApplicableHeaderTradeDelivery") {
        inv.delivery_date = date(d, &["ActualDeliverySupplyChainEvent", "OccurrenceDateTime"]);
    }
    if let Some(s) = child(tx, "ApplicableHeaderTradeSettlement") {
        inv.currency = txt(s, &["InvoiceCurrencyCode"]);
        inv.payment_reference = txt(s, &["PaymentReference"]);
        for t in children(s, "ApplicableTradeTax") {
            inv.taxes.push(Tax {
                category: txt(t, &["CategoryCode"]),
                rate: num(t, &["RateApplicablePercent"]),
                basis: num(t, &["BasisAmount"]),
                amount: num(t, &["CalculatedAmount"]),
                exemption: txt(t, &["ExemptionReason"]),
            });
        }
        if let Some(p) = child(s, "BillingSpecifiedPeriod") {
            inv.period = Some((date(p, &["StartDateTime"]), date(p, &["EndDateTime"])));
        }
        for means in children(s, "SpecifiedTradeSettlementPaymentMeans") {
            let code = txt(means, &["TypeCode"]);
            if !code.is_empty() {
                inv.payment_means.push(code);
            }
            if let Some(acc) = child(means, "PayeePartyCreditorFinancialAccount") {
                let iban = txt(acc, &["IBANID"]);
                if !iban.is_empty() {
                    inv.iban.push(iban);
                }
                inv.account_name = txt(acc, &["AccountName"]);
            }
            let bic = txt(means, &["PayeeSpecifiedCreditorFinancialInstitution", "BICID"]);
            if !bic.is_empty() {
                inv.bic = bic;
            }
        }
        for terms in children(s, "SpecifiedTradePaymentTerms") {
            let d = txt(terms, &["Description"]);
            if !d.is_empty() {
                inv.payment_terms.push(d);
            }
            let due = date(terms, &["DueDateDateTime"]);
            if !due.is_empty() {
                inv.due_date = due;
            }
        }
        if let Some(m) = child(s, "SpecifiedTradeSettlementHeaderMonetarySummation") {
            inv.totals = Totals {
                lines: num(m, &["LineTotalAmount"]),
                charges: num(m, &["ChargeTotalAmount"]),
                allowances: num(m, &["AllowanceTotalAmount"]),
                tax_basis: num(m, &["TaxBasisTotalAmount"]),
                tax: children(m, "TaxTotalAmount")
                    .find(|n| n.attribute("currencyID").is_none_or(|c| c == inv.currency || inv.currency.is_empty()))
                    .and_then(|n| text(Some(n)).parse().ok()),
                grand: num(m, &["GrandTotalAmount"]),
                prepaid: num(m, &["TotalPrepaidAmount"]),
                rounding: num(m, &["RoundingAmount"]),
                due: num(m, &["DuePayableAmount"]),
            };
        }
    }
    Ok(inv)
}

/// Short name of a ZUGFeRD / Factur-X / XRechnung profile URN.
pub fn profile_name(urn: &str) -> &'static str {
    let u = urn.to_lowercase();
    if u.contains("xrechnung") {
        "XRechnung"
    } else if u.contains("extended") {
        "EXTENDED"
    } else if u.contains("en16931") && !u.contains("basic") {
        "EN 16931 (COMFORT)"
    } else if u.contains("basicwl") {
        "BASIC WL"
    } else if u.contains("basic") {
        "BASIC"
    } else if u.contains("minimum") {
        "MINIMUM"
    } else {
        "?"
    }
}

/// Invoice type codes (UNTDID 1001) people meet.
pub fn type_name(code: &str) -> &'static str {
    match code {
        "380" => "Invoice",
        "381" => "Credit note",
        "384" => "Corrected invoice",
        "389" => "Self-billed invoice",
        "326" => "Partial invoice",
        "875" | "876" | "877" => "Partial construction invoice",
        "751" => "Invoice information for accounting purposes",
        _ => "Invoice",
    }
}

/// Unit codes (UN/ECE Recommendation 20) people meet.
fn unit_name(code: &str) -> String {
    let name = match code {
        "C62" | "H87" | "EA" | "XPP" => "pc.",
        "HUR" => "h",
        "MIN" => "min",
        "DAY" => "days",
        "WEE" => "weeks",
        "MON" => "months",
        "ANN" => "years",
        "KGM" => "kg",
        "GRM" => "g",
        "TNE" => "t",
        "MTR" => "m",
        "KMT" => "km",
        "MTK" => "m²",
        "MTQ" => "m³",
        "LTR" => "l",
        "KWH" => "kWh",
        "LS" => "flat rate",
        "P1" => "%",
        other => return other.to_string(),
    };
    t(name).to_string()
}

/// VAT category codes (UNCL5305).
pub fn tax_category_name(code: &str) -> &'static str {
    match code {
        "S" => "Standard rate",
        "Z" => "Zero rated",
        "E" => "Exempt",
        "AE" => "Reverse charge",
        "K" => "Intra-community supply",
        "G" => "Export outside the EU",
        "O" => "Not subject to VAT",
        "L" => "Canary Islands",
        "M" => "Ceuta and Melilla",
        _ => "",
    }
}

/// One arithmetic check of the invoice data.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub ok: bool,
    pub what: String,
}

fn close(a: f64, b: f64, n: usize) -> bool {
    // Each rounded amount may be off by half a cent.
    (a - b).abs() <= 0.005 * n.max(1) as f64 + 1e-9
}

/// Fields EN 16931 (the European e-invoice standard behind ZUGFeRD, Factur-X and
/// XRechnung) requires that are missing. MINIMUM and BASIC WL profiles need less.
pub fn missing_required(inv: &Invoice) -> Vec<&'static str> {
    let reduced = matches!(inv.profile_name.as_str(), "MINIMUM" | "BASIC WL");
    let mut out = Vec::new();
    let mut need = |present: bool, what: &'static str| {
        if !present {
            out.push(what);
        }
    };
    need(!inv.number.is_empty(), "Invoice number (BT-1)");
    need(!inv.issue_date.is_empty(), "Invoice date (BT-2)");
    need(!inv.currency.is_empty(), "Currency (BT-5)");
    need(!inv.seller.name.is_empty(), "Seller name (BT-27)");
    need(!inv.buyer.name.is_empty(), "Buyer name (BT-44)");
    need(inv.totals.grand.is_some(), "Total (BT-112)");
    need(inv.totals.due.is_some(), "Amount due (BT-115)");
    if !reduced {
        need(inv.seller.address.iter().any(|l| l.ends_with(char::is_alphabetic)), "Seller country (BT-40)");
        need(!inv.lines.is_empty(), "Invoice lines (BG-25)");
        need(!inv.taxes.is_empty(), "VAT breakdown (BG-23)");
        need(!inv.payment_means.is_empty(), "Payment means (BT-81)");
        need(
            !inv.seller.vat_id.is_empty() || !inv.seller.tax_number.is_empty() || inv.taxes.iter().all(|t| t.category == "O"),
            "Seller VAT ID or tax number (BT-31/BT-32)",
        );
    }
    out
}

/// Checks that the amounts in the invoice data add up.
pub fn checks(inv: &Invoice) -> Vec<Check> {
    let mut out = Vec::new();
    let mut add = |ok: bool, what: String| out.push(Check { ok, what });
    let t = &inv.totals;
    let line_sum: Option<f64> = inv.lines.iter().map(|l| l.total).sum();
    if let (Some(sum), Some(total)) = (line_sum, t.lines)
        && !inv.lines.is_empty()
    {
        add(close(sum, total, inv.lines.len()), tf("Sum of the lines {sum} = line total {total}", &[("sum", &money(sum)), ("total", &money(total))]));
    }
    for l in &inv.lines {
        if let (Some(q), Some(p), Some(total)) = (l.quantity, l.net_price, l.total) {
            let calc = q * p / l.price_basis.filter(|b| *b > 0.0).unwrap_or(1.0);
            if !close(calc, total, 2) {
                add(
                    false,
                    tf(
                        "Line {line}: {quantity} × {price} = {calc}, not {total}",
                        &[("line", &l.id), ("quantity", &q), ("price", &money(p)), ("calc", &money(calc)), ("total", &money(total))],
                    ),
                );
            }
        }
    }
    if let (Some(lines), Some(basis)) = (t.lines, t.tax_basis) {
        let calc = lines - t.allowances.unwrap_or(0.0) + t.charges.unwrap_or(0.0);
        add(close(calc, basis, 2), tf("Taxable amount {amount} matches the lines, charges and allowances", &[("amount", &money(basis))]));
    }
    for tax in &inv.taxes {
        if let (Some(b), Some(r), Some(a)) = (tax.basis, tax.rate, tax.amount) {
            add(close(b * r / 100.0, a, 1), tf("VAT {rate}% of {basis} = {amount}", &[("rate", &number(r)), ("basis", &money(b)), ("amount", &money(a))]));
        }
    }
    let tax_sum: Option<f64> = inv.taxes.iter().map(|x| x.amount).sum();
    if let (Some(sum), Some(total)) = (tax_sum, t.tax)
        && !inv.taxes.is_empty()
    {
        add(close(sum, total, inv.taxes.len()), tf("VAT amounts add up to the VAT total {amount}", &[("amount", &money(total))]));
    }
    if let (Some(basis), Some(tax), Some(grand)) = (t.tax_basis, t.tax, t.grand) {
        add(close(basis + tax, grand, 2), tf("Taxable amount + VAT = total {amount}", &[("amount", &money(grand))]));
    }
    if let (Some(grand), Some(due)) = (t.grand, t.due) {
        let calc = grand - t.prepaid.unwrap_or(0.0) + t.rounding.unwrap_or(0.0);
        add(close(calc, due, 2), tf("Amount due {amount} = total − prepaid", &[("amount", &money(due))]));
    }
    out
}

fn number(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// An amount with two decimals and thousands separators.
pub fn money(v: f64) -> String {
    let s = format!("{:.2}", v.abs());
    let (int, dec) = s.split_once('.').unwrap_or((&s, "00"));
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push('\'');
        }
        grouped.push(c);
    }
    format!("{}{grouped}.{dec}", if v < 0.0 { "−" } else { "" })
}

/// Whether `amount` appears in `page_text`, in any of the usual ways of writing it
/// (1234.56, 1,234.56, 1.234,56, 1'234.56, 1 234,56).
pub fn amount_in_text(amount: f64, page_text: &str) -> bool {
    let cents = (amount.abs() * 100.0).round() as u64;
    // Numbers in the text as digit strings with the last two digits after a separator.
    let chars: Vec<char> = page_text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || ".,'’  ".contains(chars[i])) {
                i += 1;
            }
            let token: String = chars[start..i].iter().collect();
            let token = token.trim_end_matches(|c: char| !c.is_ascii_digit());
            // The decimal separator is the last '.' or ',' followed by exactly two digits.
            if let Some(pos) = token.rfind(['.', ',']) {
                let dec = &token[pos + 1..];
                if dec.len() == 2 && dec.chars().all(|c| c.is_ascii_digit()) {
                    let digits: String = token[..pos].chars().filter(char::is_ascii_digit).collect();
                    if let Ok(whole) = digits.parse::<u64>()
                        && whole * 100 + dec.parse::<u64>().unwrap_or(0) == cents
                    {
                        return true;
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
pub const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rsm:CrossIndustryInvoice xmlns:rsm="urn:un:unece:uncefact:data:standard:CrossIndustryInvoice:100"
  xmlns:ram="urn:un:unece:uncefact:data:standard:ReusableAggregateBusinessInformationEntity:100"
  xmlns:udt="urn:un:unece:uncefact:data:standard:UnqualifiedDataType:100">
  <rsm:ExchangedDocumentContext><ram:GuidelineSpecifiedDocumentContextParameter>
    <ram:ID>urn:cen.eu:en16931:2017#conformant#urn:factur-x.eu:1p0:extended</ram:ID>
  </ram:GuidelineSpecifiedDocumentContextParameter></rsm:ExchangedDocumentContext>
  <rsm:ExchangedDocument><ram:ID>RE-1001</ram:ID><ram:TypeCode>380</ram:TypeCode>
    <ram:IssueDateTime><udt:DateTimeString format="102">20261101</udt:DateTimeString></ram:IssueDateTime>
    <ram:IncludedNote><ram:Content>Thank you for your order.</ram:Content></ram:IncludedNote>
  </rsm:ExchangedDocument>
  <rsm:SupplyChainTradeTransaction>
    <ram:IncludedSupplyChainTradeLineItem>
      <ram:AssociatedDocumentLineDocument><ram:LineID>1</ram:LineID></ram:AssociatedDocumentLineDocument>
      <ram:SpecifiedTradeProduct><ram:SellerAssignedID>W-7</ram:SellerAssignedID><ram:Name>Water</ram:Name></ram:SpecifiedTradeProduct>
      <ram:SpecifiedLineTradeAgreement><ram:NetPriceProductTradePrice><ram:ChargeAmount>2.50</ram:ChargeAmount></ram:NetPriceProductTradePrice></ram:SpecifiedLineTradeAgreement>
      <ram:SpecifiedLineTradeDelivery><ram:BilledQuantity unitCode="MTQ">40</ram:BilledQuantity></ram:SpecifiedLineTradeDelivery>
      <ram:SpecifiedLineTradeSettlement>
        <ram:ApplicableTradeTax><ram:TypeCode>VAT</ram:TypeCode><ram:CategoryCode>S</ram:CategoryCode><ram:RateApplicablePercent>7</ram:RateApplicablePercent></ram:ApplicableTradeTax>
        <ram:SpecifiedTradeSettlementLineMonetarySummation><ram:LineTotalAmount>100.00</ram:LineTotalAmount></ram:SpecifiedTradeSettlementLineMonetarySummation>
      </ram:SpecifiedLineTradeSettlement>
    </ram:IncludedSupplyChainTradeLineItem>
    <ram:IncludedSupplyChainTradeLineItem>
      <ram:AssociatedDocumentLineDocument><ram:LineID>2</ram:LineID></ram:AssociatedDocumentLineDocument>
      <ram:SpecifiedTradeProduct><ram:Name>Meter rent</ram:Name></ram:SpecifiedTradeProduct>
      <ram:SpecifiedLineTradeAgreement><ram:NetPriceProductTradePrice><ram:ChargeAmount>1100.00</ram:ChargeAmount></ram:NetPriceProductTradePrice></ram:SpecifiedLineTradeAgreement>
      <ram:SpecifiedLineTradeDelivery><ram:BilledQuantity unitCode="C62">1</ram:BilledQuantity></ram:SpecifiedLineTradeDelivery>
      <ram:SpecifiedLineTradeSettlement>
        <ram:ApplicableTradeTax><ram:TypeCode>VAT</ram:TypeCode><ram:CategoryCode>S</ram:CategoryCode><ram:RateApplicablePercent>19</ram:RateApplicablePercent></ram:ApplicableTradeTax>
        <ram:SpecifiedTradeSettlementLineMonetarySummation><ram:LineTotalAmount>1100.00</ram:LineTotalAmount></ram:SpecifiedTradeSettlementLineMonetarySummation>
      </ram:SpecifiedLineTradeSettlement>
    </ram:IncludedSupplyChainTradeLineItem>
    <ram:ApplicableHeaderTradeAgreement>
      <ram:SellerTradeParty><ram:Name>Stadtwerke Beispiel GmbH</ram:Name>
        <ram:PostalTradeAddress><ram:PostcodeCode>12345</ram:PostcodeCode><ram:LineOne>Hauptstr. 1</ram:LineOne><ram:CityName>Musterstadt</ram:CityName><ram:CountryID>DE</ram:CountryID></ram:PostalTradeAddress>
        <ram:SpecifiedTaxRegistration><ram:ID schemeID="VA">DE123456789</ram:ID></ram:SpecifiedTaxRegistration>
      </ram:SellerTradeParty>
      <ram:BuyerTradeParty><ram:Name>Erika Muster</ram:Name></ram:BuyerTradeParty>
    </ram:ApplicableHeaderTradeAgreement>
    <ram:ApplicableHeaderTradeDelivery/>
    <ram:ApplicableHeaderTradeSettlement>
      <ram:InvoiceCurrencyCode>EUR</ram:InvoiceCurrencyCode>
      <ram:SpecifiedTradeSettlementPaymentMeans><ram:TypeCode>58</ram:TypeCode>
        <ram:PayeePartyCreditorFinancialAccount><ram:IBANID>DE02120300000000202051</ram:IBANID></ram:PayeePartyCreditorFinancialAccount>
      </ram:SpecifiedTradeSettlementPaymentMeans>
      <ram:ApplicableTradeTax><ram:CalculatedAmount>7.00</ram:CalculatedAmount><ram:TypeCode>VAT</ram:TypeCode><ram:BasisAmount>100.00</ram:BasisAmount><ram:CategoryCode>S</ram:CategoryCode><ram:RateApplicablePercent>7</ram:RateApplicablePercent></ram:ApplicableTradeTax>
      <ram:ApplicableTradeTax><ram:CalculatedAmount>209.00</ram:CalculatedAmount><ram:TypeCode>VAT</ram:TypeCode><ram:BasisAmount>1100.00</ram:BasisAmount><ram:CategoryCode>S</ram:CategoryCode><ram:RateApplicablePercent>19</ram:RateApplicablePercent></ram:ApplicableTradeTax>
      <ram:BillingSpecifiedPeriod><ram:StartDateTime><udt:DateTimeString format="102">20261001</udt:DateTimeString></ram:StartDateTime><ram:EndDateTime><udt:DateTimeString format="102">20261031</udt:DateTimeString></ram:EndDateTime></ram:BillingSpecifiedPeriod>
      <ram:SpecifiedTradePaymentTerms><ram:Description>Payable within 14 days</ram:Description><ram:DueDateDateTime><udt:DateTimeString format="102">20261115</udt:DateTimeString></ram:DueDateDateTime></ram:SpecifiedTradePaymentTerms>
      <ram:SpecifiedTradeSettlementHeaderMonetarySummation>
        <ram:LineTotalAmount>1200.00</ram:LineTotalAmount><ram:TaxBasisTotalAmount>1200.00</ram:TaxBasisTotalAmount>
        <ram:TaxTotalAmount currencyID="EUR">216.00</ram:TaxTotalAmount><ram:GrandTotalAmount>1416.00</ram:GrandTotalAmount>
        <ram:DuePayableAmount>1416.00</ram:DuePayableAmount>
      </ram:SpecifiedTradeSettlementHeaderMonetarySummation>
    </ram:ApplicableHeaderTradeSettlement>
  </rsm:SupplyChainTradeTransaction>
</rsm:CrossIndustryInvoice>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_factur_x_invoice() {
        assert!(is_invoice_xml(SAMPLE.as_bytes()));
        let inv = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(inv.profile_name, "EXTENDED");
        assert_eq!((inv.number.as_str(), type_name(&inv.type_code)), ("RE-1001", "Invoice"));
        assert_eq!(inv.issue_date, "2026-11-01");
        assert_eq!(inv.seller.name, "Stadtwerke Beispiel GmbH");
        assert_eq!(inv.seller.address, ["Hauptstr. 1", "12345 Musterstadt", "DE"]);
        assert_eq!(inv.seller.vat_id, "DE123456789");
        assert_eq!(inv.lines.len(), 2);
        assert_eq!((inv.lines[0].quantity, inv.lines[0].unit.as_str()), (Some(40.0), "m³"));
        assert_eq!(inv.lines[0].article, "W-7");
        assert_eq!(inv.taxes.len(), 2);
        assert_eq!(inv.period, Some(("2026-10-01".into(), "2026-10-31".into())));
        assert_eq!(inv.due_date, "2026-11-15");
        assert_eq!(inv.iban, ["DE02120300000000202051"]);
        assert_eq!(inv.totals.grand, Some(1416.0));
        assert_eq!(inv.notes, ["Thank you for your order."]);
        assert_eq!(inv.payment_means, ["58"]);
        assert!(missing_required(&inv).is_empty(), "{:?}", missing_required(&inv));
        let mut bad = inv.clone();
        bad.payment_means.clear();
        bad.seller.vat_id.clear();
        assert_eq!(missing_required(&bad), ["Payment means (BT-81)", "Seller VAT ID or tax number (BT-31/BT-32)"]);
    }

    #[test]
    fn arithmetic_checks_pass_and_catch_errors() {
        let inv = parse(SAMPLE.as_bytes()).unwrap();
        let c = checks(&inv);
        assert!(c.len() >= 6, "{c:?}");
        assert!(c.iter().all(|c| c.ok), "{c:?}");
        let mut bad = inv.clone();
        bad.totals.grand = Some(1417.0);
        bad.lines[1].total = Some(1000.0);
        let failed: Vec<_> = checks(&bad).into_iter().filter(|c| !c.ok).map(|c| c.what).collect();
        assert!(failed.iter().any(|w| w.contains("Sum of the lines")), "{failed:?}");
        assert!(failed.iter().any(|w| w.contains("= total")), "{failed:?}");
        assert!(failed.iter().any(|w| w.starts_with("Line 2")), "{failed:?}");
    }

    #[test]
    fn amounts_are_found_however_they_are_written() {
        for text in ["Total 1416.00 EUR", "Summe: 1.416,00 €", "1'416.00", "1 416,00", "EUR 1,416.00"] {
            assert!(amount_in_text(1416.0, text), "{text}");
        }
        assert!(!amount_in_text(1416.0, "1416 items, 14.16 each, 11416.00"));
        assert_eq!(money(1234567.5), "1'234'567.50");
    }

    #[test]
    fn other_formats_are_reported() {
        assert!(parse(b"<Invoice xmlns='urn:oasis:names:specification:ubl:schema:xsd:Invoice-2'/>").is_err());
        assert!(parse(b"not xml").is_err());
    }
}
