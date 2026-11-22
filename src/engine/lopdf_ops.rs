//! Changes PDFium has no API for, done on the saved bytes with lopdf: writing the
//! bookmark tree (outline) and lossless compression.

use lopdf::{Dictionary, Document, Object, ObjectId, StringFormat};

use super::commands::OutlineItem;

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
/// packs objects into compressed object streams.
pub fn compress(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut doc = load(bytes)?;
    doc.delete_zero_length_streams();
    doc.prune_objects();
    doc.compress();
    save(&mut doc, true)
}
