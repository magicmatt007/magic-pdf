//! Changes PDFium has no API for, done on the saved bytes with lopdf: writing the
//! bookmark tree (outline), lossless compression and clearing out what redaction
//! left unreferenced.

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

/// The file's bookmark dictionaries in reading order (depth first), as PDFium reads
/// them: `OutlineOrigin::index` points into this list.
fn old_outline(doc: &Document) -> Vec<Dictionary> {
    const MAX_ITEMS: usize = 20_000;
    const MAX_DEPTH: usize = 64;
    fn walk(
        doc: &Document,
        first: Option<ObjectId>,
        depth: usize,
        out: &mut Vec<Dictionary>,
        seen: &mut std::collections::HashSet<ObjectId>,
    ) {
        let mut next = first;
        while let Some(id) = next {
            if out.len() >= MAX_ITEMS || !seen.insert(id) {
                return;
            }
            let Ok(d) = doc.get_dictionary(id) else { return };
            out.push(d.clone());
            let child = d.get(b"First").and_then(Object::as_reference).ok();
            if depth < MAX_DEPTH {
                walk(doc, child, depth + 1, out, seen);
            }
            next = d.get(b"Next").and_then(Object::as_reference).ok();
        }
    }
    let first = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"Outlines").ok())
        .and_then(|o| o.as_reference().ok())
        .and_then(|r| doc.get_dictionary(r).ok())
        .and_then(|o| o.get(b"First").and_then(Object::as_reference).ok());
    let mut out = Vec::new();
    walk(doc, first, 0, &mut out, &mut std::collections::HashSet::new());
    out
}

/// Replaces the document's bookmarks with `items`.
pub fn write_outline(bytes: &[u8], items: &[OutlineItem]) -> Result<Vec<u8>, String> {
    let mut doc = load(bytes)?;
    let pages: Vec<ObjectId> = doc.get_pages().into_values().collect();
    let old = old_outline(&doc);
    let root = if items.is_empty() {
        None
    } else {
        let root_id = doc.new_object_id();
        let (first, last, count) = add_items(&mut doc, &pages, &old, items, root_id);
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
fn add_items(
    doc: &mut Document,
    pages: &[ObjectId],
    old: &[Dictionary],
    items: &[OutlineItem],
    parent: ObjectId,
) -> (ObjectId, ObjectId, i64) {
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
        // What magic-pdf does not edit comes from the bookmark it was read as.
        let was = item.origin.and_then(|o| Some((old.get(o.index)?, o.page)));
        for key in [b"C".as_slice(), b"F"] {
            if let Some(v) = was.and_then(|(w, _)| w.get(key).ok()) {
                d.set(key.to_vec(), v.clone());
            }
        }
        let kept_target = match was {
            // Not re-pointed: the original target (a spot on the page, a named
            // destination, a web link, …).
            Some((w, page)) if page == item.page && (w.has(b"Dest") || w.has(b"A")) => {
                for key in [b"Dest".as_slice(), b"A"] {
                    if let Ok(v) = w.get(key) {
                        d.set(key.to_vec(), v.clone());
                    }
                }
                true
            }
            _ => false,
        };
        if !kept_target && let Some(&page) = item.page.and_then(|p| pages.get(p)) {
            d.set("Dest", Object::Array(vec![Object::Reference(page), Object::Name(b"Fit".to_vec())]));
        }
        visible += 1;
        if !item.children.is_empty() {
            let (first, last, count) = add_items(doc, pages, old, &item.children, ids[i]);
            d.set("First", Object::Reference(first));
            d.set("Last", Object::Reference(last));
            // Positive: shown expanded; negative: collapsed (as it was).
            let collapsed = was.and_then(|(w, _)| w.get(b"Count").ok()?.as_i64().ok()).is_some_and(|c| c < 0);
            if collapsed {
                d.set("Count", Object::Integer(-count));
            } else {
                d.set("Count", Object::Integer(count));
                visible += count;
            }
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
    // Empty streams stay: an empty appearance is often deliberate (an annotation that
    // draws nothing, an empty form field), and removing it lets viewers draw a default.
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
    for (key, value) in [("Title", &p.title), ("Author", &p.author), ("Subject", &p.subject), ("Keywords", &p.keywords)]
    {
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

/// After redaction: drops XObjects the redacted pages no longer draw from their
/// resources, then every object nothing refers to any more (old content streams,
/// images and annotation appearances that could still hold the removed content).
pub fn purge_after_redaction(bytes: &[u8], pages: &[usize]) -> Result<Vec<u8>, String> {
    let mut doc = load(bytes)?;
    let ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    for &i in pages {
        let Some(&page_id) = ids.get(i) else { continue };
        let content = doc.get_page_content(page_id);
        let ops =
            lopdf::content::Content::decode(&content).map_err(|e| format!("could not read the page content: {e}"))?;
        let used: Vec<Vec<u8>> = ops
            .operations
            .iter()
            .filter(|op| op.operator == "Do")
            .filter_map(|op| op.operands.first()?.as_name().ok().map(<[u8]>::to_vec))
            .collect();
        // The page's own resources, or inherited ones, as a direct dictionary.
        let resources = match doc.get_page_resources(page_id) {
            Ok((Some(d), _)) => d.clone(),
            Ok((None, refs)) => match refs.first().and_then(|r| doc.get_dictionary(*r).ok()) {
                Some(d) => d.clone(),
                None => continue,
            },
            Err(_) => continue,
        };
        let mut resources = resources;
        let xobjects = match resources.get(b"XObject") {
            Ok(Object::Dictionary(d)) => Some(d.clone()),
            Ok(Object::Reference(r)) => doc.get_dictionary(*r).ok().cloned(),
            _ => None,
        };
        let Some(mut xobjects) = xobjects else { continue };
        let unused: Vec<Vec<u8>> = xobjects.iter().map(|(k, _)| k.clone()).filter(|k| !used.contains(k)).collect();
        if unused.is_empty() {
            continue;
        }
        for k in unused {
            xobjects.remove(&k);
        }
        resources.set("XObject", Object::Dictionary(xobjects));
        let page = doc
            .get_object_mut(page_id)
            .and_then(Object::as_dict_mut)
            .map_err(|e| format!("could not update the page: {e}"))?;
        page.set("Resources", Object::Dictionary(resources));
    }
    doc.prune_objects();
    save(&mut doc, false)
}

/// Private key holding magic-pdf's replies (JSON), as written by PDFium.
pub const REPLIES_KEY: &str = "MagicPdfReplies";
/// Marks the standard reply annotations magic-pdf writes from those.
pub const REPLY_COPY_KEY: &str = "MagicPdfReply";
/// Private key holding magic-pdf's review status of an annotation.
pub const STATUS_KEY: &str = "MagicPdfStatus";

/// Writes magic-pdf's replies as standard reply annotations (/IRT), which other
/// applications show in the annotation's comment thread. Copies written before are
/// replaced, so they always match magic-pdf's own list. Returns `bytes` unchanged
/// when there is nothing to do.
/// With `incremental`, only the changed objects are appended to `bytes` (for signed
/// files: the signed bytes stay as they are).
pub fn write_standard_replies(bytes: &[u8], incremental: bool) -> Result<Vec<u8>, String> {
    // "MagicPdfRepl" starts the reply keys.
    let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    if !has(b"MagicPdfRepl") && !has(STATUS_KEY.as_bytes()) {
        return Ok(bytes.to_vec()); // no replies, statuses or copies of them
    }
    if incremental {
        return rewrite_incrementally(bytes, standard_replies);
    }
    let mut doc = load(bytes)?;
    standard_replies(&mut doc);
    save(&mut doc, false)
}

/// Applies `change` to the document and appends the objects it changed or added to
/// `bytes` as an incremental update.
fn rewrite_incrementally(bytes: &[u8], change: impl FnOnce(&mut Document)) -> Result<Vec<u8>, String> {
    let before = load(bytes)?;
    let mut after = before.clone();
    change(&mut after);
    let changed: Vec<(ObjectId, Object)> = after
        .objects
        .iter()
        .filter(|(id, obj)| before.objects.get(id) != Some(*obj))
        .map(|(id, obj)| (*id, obj.clone()))
        .collect();
    if changed.is_empty() {
        return Ok(bytes.to_vec());
    }
    let mut inc = lopdf::IncrementalDocument::create_from(bytes.to_vec(), before);
    for (id, obj) in changed {
        inc.new_document.objects.insert(id, obj);
    }
    inc.new_document.max_id = inc.new_document.max_id.max(after.max_id);
    let mut out = Vec::new();
    inc.save_to(&mut out).map_err(|e| format!("could not write the PDF: {e}"))?;
    Ok(out)
}

fn standard_replies(doc: &mut Document) {
    let pages: Vec<ObjectId> = doc.get_pages().into_values().collect();
    for page_id in pages {
        let annots_obj = match doc.get_dictionary(page_id).ok().and_then(|p| p.get(b"Annots").ok()).cloned() {
            Some(o) => o,
            None => continue,
        };
        // The /Annots array may be in the page or an object of its own.
        let (array_id, refs) = match &annots_obj {
            Object::Reference(id) => {
                (Some(*id), doc.get_object(*id).and_then(Object::as_array).cloned().unwrap_or_default())
            }
            Object::Array(a) => (None, a.clone()),
            _ => continue,
        };
        let mut keep = Vec::new();
        let mut parents = Vec::new();
        for r in refs {
            let Ok(id) = r.as_reference() else {
                keep.push(r);
                continue;
            };
            let Ok(dict) = doc.get_dictionary(id) else {
                keep.push(r);
                continue;
            };
            if dict.has(REPLY_COPY_KEY.as_bytes()) {
                doc.objects.remove(&id); // an earlier copy: written anew below
                continue;
            }
            // A text string: UTF-16 or PDFDocEncoding (where € or – are not Latin-1).
            let replies: Vec<super::commands::Reply> = dict
                .get(REPLIES_KEY.as_bytes())
                .and_then(lopdf::decode_text_string)
                .ok()
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default();
            let status = dict
                .get(STATUS_KEY.as_bytes())
                .and_then(lopdf::decode_text_string)
                .ok()
                .filter(|s| super::commands::STATUSES[1..].contains(&s.as_str()));
            if !replies.is_empty() || status.is_some() {
                parents.push((id, dict.get(b"Rect").ok().cloned(), replies, status));
            }
            keep.push(r);
        }
        for (parent, rect, replies, status) in parents {
            // The review status as other applications store it: a hidden state
            // annotation in reply to the annotation (state model "Review").
            if let Some(state) = status {
                let mut d = Dictionary::new();
                d.set("Type", Object::Name(b"Annot".to_vec()));
                d.set("Subtype", Object::Name(b"Text".to_vec()));
                if let Some(rect) = &rect {
                    d.set("Rect", rect.clone());
                }
                d.set("IRT", Object::Reference(parent));
                d.set("P", Object::Reference(page_id));
                // Hidden, print, no zoom, no rotation (as Acrobat writes states).
                d.set("F", 30);
                d.set("StateModel", text_string("Review"));
                d.set("State", text_string(&state));
                d.set("Contents", text_string(&state));
                d.set("NM", text_string(&format!("magic-pdf-state-{}-{}", parent.0, parent.1)));
                d.set(REPLY_COPY_KEY, text_string("1"));
                keep.push(Object::Reference(doc.add_object(d)));
            }
            for (n, reply) in replies.iter().enumerate() {
                let appearance = doc.add_object(lopdf::Stream::new(
                    lopdf::dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 1.into(), 1.into()] },
                    Vec::new(),
                ));
                let mut d = Dictionary::new();
                d.set("Type", Object::Name(b"Annot".to_vec()));
                d.set("Subtype", Object::Name(b"Text".to_vec()));
                if let Some(rect) = &rect {
                    d.set("Rect", rect.clone());
                }
                d.set("IRT", Object::Reference(parent));
                d.set("RT", Object::Name(b"R".to_vec()));
                d.set("P", Object::Reference(page_id));
                // Print, no zoom, no rotation, as Acrobat writes replies; drawn as nothing.
                d.set("F", 28);
                d.set("Name", Object::Name(b"Comment".to_vec()));
                d.set("Open", false);
                d.set("T", text_string(&reply.author));
                d.set("Contents", text_string(&reply.text));
                d.set("M", text_string(&reply.date));
                d.set("CreationDate", text_string(&reply.date));
                d.set("NM", text_string(&format!("magic-pdf-reply-{}-{}-{n}", parent.0, parent.1)));
                d.set("AP", lopdf::dictionary! { "N" => appearance });
                d.set(REPLY_COPY_KEY, text_string("1"));
                keep.push(Object::Reference(doc.add_object(d)));
            }
        }
        match array_id {
            Some(id) => {
                doc.objects.insert(id, Object::Array(keep));
            }
            None => {
                if let Ok(page) = doc.get_object_mut(page_id).and_then(Object::as_dict_mut) {
                    page.set("Annots", Object::Array(keep));
                }
            }
        }
    }
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
