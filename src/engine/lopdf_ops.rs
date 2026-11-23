//! Changes PDFium has no API for, done on the saved bytes with lopdf: writing the
//! bookmark tree (outline) and lossless compression.

use lopdf::{Dictionary, Document, Object, ObjectId, StringFormat};

use super::commands::{EditableProperties, OutlineItem};

fn load(bytes: &[u8]) -> Result<Document, String> {
    let doc = Document::load_mem(bytes).map_err(|e| format!("could not read the PDF structure: {e}"))?;
    if doc.is_encrypted() {
        return Err("this does not work for encrypted PDFs".into());
    }
    Ok(doc)
}

fn save(doc: &mut Document, modern: bool) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let r = if modern { doc.save_modern(&mut out) } else { doc.save_to(&mut out) };
    r.map_err(|e| format!("could not write the PDF: {e}"))?;
    Ok(out)
}

/// A PDF text string: plain for ASCII, UTF-16 with byte order mark otherwise.
fn text_string(s: &str) -> Object {
    if s.is_ascii() {
        Object::String(s.as_bytes().to_vec(), StringFormat::Literal)
    } else {
        let mut b = vec![0xFE, 0xFF];
        for u in s.encode_utf16() {
            b.extend(u.to_be_bytes());
        }
        Object::String(b, StringFormat::Hexadecimal)
    }
}

/// Replaces the document's bookmarks with `items`.
pub fn write_outline(bytes: &[u8], items: &[OutlineItem]) -> Result<Vec<u8>, String> {
    let mut doc = load(bytes)?;
    let pages: Vec<ObjectId> = doc.get_pages().into_values().collect();
    let root = if items.is_empty() {
        None
    } else {
        let root_id = doc.new_object_id();
        let (first, last, count) = add_items(&mut doc, &pages, items, root_id);
        let mut root = Dictionary::new();
        root.set("Type", Object::Name(b"Outlines".to_vec()));
        root.set("First", Object::Reference(first));
        root.set("Last", Object::Reference(last));
        root.set("Count", Object::Integer(count));
        doc.objects.insert(root_id, Object::Dictionary(root));
        Some(root_id)
    };
    let catalog = doc.catalog_mut().map_err(|e| format!("no document catalog: {e}"))?;
    match root {
        Some(id) => catalog.set("Outlines", Object::Reference(id)),
        None => {
            catalog.remove(b"Outlines");
        }
    }
    // The old bookmark objects are no longer referenced.
    doc.prune_objects();
    save(&mut doc, false)
}

/// Adds `items` as children of `parent`; returns the first and last child and the
/// number of visible descendants.
fn add_items(doc: &mut Document, pages: &[ObjectId], items: &[OutlineItem], parent: ObjectId) -> (ObjectId, ObjectId, i64) {
    let ids: Vec<ObjectId> = items.iter().map(|_| doc.new_object_id()).collect();
    let mut visible = 0;
    for (i, item) in items.iter().enumerate() {
        let mut d = Dictionary::new();
        d.set("Title", text_string(&item.title));
        d.set("Parent", Object::Reference(parent));
        if i > 0 {
            d.set("Prev", Object::Reference(ids[i - 1]));
        }
        if i + 1 < ids.len() {
            d.set("Next", Object::Reference(ids[i + 1]));
        }
        if let Some(&page) = item.page.and_then(|p| pages.get(p)) {
            d.set("Dest", Object::Array(vec![Object::Reference(page), Object::Name(b"Fit".to_vec())]));
        }
        visible += 1;
        if !item.children.is_empty() {
            let (first, last, count) = add_items(doc, pages, &item.children, ids[i]);
            d.set("First", Object::Reference(first));
            d.set("Last", Object::Reference(last));
            // Positive: shown expanded.
            d.set("Count", Object::Integer(count));
            visible += count;
        }
        doc.objects.insert(ids[i], Object::Dictionary(d));
    }
    (ids[0], ids[ids.len() - 1], visible)
}

/// Lossless size reduction: compresses uncompressed streams, drops unused objects and
/// packs objects into compressed object streams. XMP metadata stays uncompressed
/// (PDF/A, e.g. e-invoices, requires that).
pub fn compress(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut doc = load(bytes)?;
    doc.delete_zero_length_streams();
    doc.prune_objects();
    for obj in doc.objects.values_mut() {
        if let Object::Stream(stream) = obj {
            let is_metadata = stream.dict.get(b"Type").and_then(|t| t.as_name()).is_ok_and(|n| n == b"Metadata");
            if !is_metadata && stream.dict.get(b"Filter").is_err() {
                let _ = stream.compress();
            }
        }
    }
    save(&mut doc, true)
}

/// "D:20261123101500Z" as an XMP date "2026-11-23T10:15:00Z".
fn xmp_date(pdf: &str) -> String {
    let d: String = pdf.trim_start_matches("D:").chars().filter(char::is_ascii_digit).take(14).collect();
    if d.len() < 14 {
        return String::new();
    }
    format!("{}-{}-{}T{}:{}:{}Z", &d[0..4], &d[4..6], &d[6..8], &d[8..10], &d[10..12], &d[12..14])
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Replaces the text of the first `<inner …>` element inside `<outer …>…</outer>`
/// (or of `<outer>` itself when `inner` is None). Returns false if not found.
fn set_xml_text(xml: &mut String, outer: &str, inner: Option<&str>, value: &str) -> bool {
    let Some(start) = xml.find(&format!("<{outer}")) else { return false };
    let Some(end) = xml[start..].find(&format!("</{outer}>")).map(|e| start + e) else { return false };
    let (open, close) = match inner {
        Some(tag) => (format!("<{tag}"), format!("</{tag}>")),
        None => (format!("<{outer}"), format!("</{outer}>")),
    };
    let Some(o) = xml[start..end + close.len()].find(&open).map(|o| start + o) else { return false };
    let Some(gt) = xml[o..].find('>').map(|g| o + g + 1) else { return false };
    if xml[..gt].ends_with("/>") {
        return false;
    }
    let Some(c) = xml[gt..].find(&close).map(|c| gt + c) else { return false };
    xml.replace_range(gt..c, &xml_escape(value));
    true
}

/// Updates the XMP metadata to match the new properties, where it has those fields.
fn update_xmp(xml: &mut String, p: &EditableProperties, modified: &str) {
    set_xml_text(xml, "dc:title", Some("rdf:li"), &p.title);
    set_xml_text(xml, "dc:creator", Some("rdf:li"), &p.author);
    set_xml_text(xml, "dc:description", Some("rdf:li"), &p.subject);
    set_xml_text(xml, "pdf:Keywords", None, &p.keywords);
    let date = xmp_date(modified);
    if !date.is_empty() {
        set_xml_text(xml, "xmp:ModifyDate", None, &date);
        set_xml_text(xml, "xmp:MetadataDate", None, &date);
    }
}

/// Sets title, author, subject and keywords (and the modification date) in the
/// document information and in the XMP metadata if it has those fields.
pub fn write_properties(bytes: &[u8], p: &EditableProperties, now: &str) -> Result<Vec<u8>, String> {
    let mut doc = load(bytes)?;
    let mut info = match doc.trailer.get(b"Info") {
        Ok(Object::Reference(id)) => doc.get_dictionary(*id).cloned().unwrap_or_default(),
        Ok(Object::Dictionary(d)) => d.clone(),
        _ => Dictionary::new(),
    };
    for (key, value) in [("Title", &p.title), ("Author", &p.author), ("Subject", &p.subject), ("Keywords", &p.keywords)] {
        if value.is_empty() {
            info.remove(key.as_bytes());
        } else {
            info.set(key, text_string(value));
        }
    }
    info.set("ModDate", Object::String(now.as_bytes().to_vec(), StringFormat::Literal));
    let id = doc.add_object(Object::Dictionary(info));
    doc.trailer.set("Info", Object::Reference(id));

    let metadata = doc.catalog().ok().and_then(|c| c.get(b"Metadata").ok()).and_then(|m| m.as_reference().ok());
    if let Some(mid) = metadata
        && let Ok(Object::Stream(stream)) = doc.get_object_mut(mid)
    {
        let content = stream.decompressed_content().unwrap_or_else(|_| stream.content.clone());
        if let Ok(mut xml) = String::from_utf8(content) {
            update_xmp(&mut xml, p, now);
            // XMP metadata stays readable as plain text (PDF/A requires that).
            stream.dict.remove(b"Filter");
            stream.dict.remove(b"DecodeParms");
            stream.set_content(xml.into_bytes());
        }
    }
    doc.prune_objects();
    save(&mut doc, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xmp_fields_are_updated_in_place() {
        let mut xml = r#"<x:xmpmeta><rdf:RDF><rdf:Description>
<dc:title><rdf:Alt><rdf:li xml:lang="x-default">Old</rdf:li></rdf:Alt></dc:title>
<dc:creator><rdf:Seq><rdf:li>Someone</rdf:li></rdf:Seq></dc:creator>
<pdf:Keywords>a, b</pdf:Keywords>
<xmp:ModifyDate>2020-01-01T00:00:00Z</xmp:ModifyDate>
</rdf:Description></rdf:RDF></x:xmpmeta>"#
            .to_string();
        let p = EditableProperties {
            title: "Neu & besser".into(),
            author: "Matt".into(),
            subject: "Not in the XMP".into(),
            keywords: "x".into(),
        };
        update_xmp(&mut xml, &p, "D:20261123101500Z");
        assert!(xml.contains(r#"<rdf:li xml:lang="x-default">Neu &amp; besser</rdf:li>"#), "{xml}");
        assert!(xml.contains("<rdf:li>Matt</rdf:li>"));
        assert!(xml.contains("<pdf:Keywords>x</pdf:Keywords>"));
        assert!(xml.contains("<xmp:ModifyDate>2026-11-23T10:15:00Z</xmp:ModifyDate>"));
        assert!(!xml.contains("Not in the XMP"), "missing fields are not added");
    }
}
