//! Signing PDFs: a detached CMS signature (PAdES, `ETSI.CAdES.detached`) in a visible
//! signature field, appended to the file as an incremental update, so the bytes
//! before it (and any earlier signatures) stay as they are.
//!
//! The key comes from a certificate file (.p12/.pfx). The CMS structure is written
//! with the small DER writer below; the RSA and ECDSA maths come from the RustCrypto
//! crates. A trusted timestamp (RFC 3161) can be added from a timestamp server.

use crate::i18n::{t, tf};
use std::fmt::Write as _;

use lopdf::{Dictionary, Document, IncrementalDocument, Object, ObjectId, Stream, StringFormat};
use sha2::{Digest, Sha256, Sha384};

use super::signatures::{Certificate, Tlv};

/// A private key with its certificate chain (the signer's certificate first).
pub struct SigningIdentity {
    key: Key,
    chain: Vec<Vec<u8>>,
    /// The signer's common name (or organisation), from the certificate.
    pub name: String,
    /// The certificate's issuer.
    pub issuer: String,
    /// The certificate was issued by the signer itself.
    pub self_signed: bool,
    /// Last day the certificate is valid (YYYY-MM-DD).
    pub expires: String,
    /// Today lies within the certificate's validity.
    pub valid_now: bool,
    /// The certificate is meant for signing.
    pub for_signing: bool,
}

enum Key {
    Rsa(Box<rsa::pkcs1v15::SigningKey<Sha256>>),
    P256(p256::ecdsa::SigningKey),
    P384(p384::ecdsa::SigningKey),
    /// A key kept elsewhere (the Windows certificate store, a smart card).
    #[cfg_attr(not(windows), allow(dead_code))]
    External(Box<dyn ExternalKey>),
}

/// The kind of key, which decides the digest and the signature format.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyKind {
    Rsa,
    P256,
    P384,
}

/// A private key that signs digests itself (e.g. in the Windows certificate store).
pub trait ExternalKey: Send + Sync {
    fn kind(&self) -> KeyKind;
    /// Signs a digest (SHA-256, or SHA-384 for P-384): for RSA a PKCS #1 v1.5
    /// signature, for ECDSA the raw r and s values one after the other.
    fn sign_digest(&self, digest: &[u8]) -> Result<Vec<u8>, String>;
}

impl std::fmt::Debug for SigningIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SigningIdentity({})", self.name)
    }
}

/// Why a certificate file could not be used (the UI words these).
#[derive(Clone, Debug, PartialEq)]
pub enum IdentityError {
    /// The password is wrong (or the file is not a certificate file).
    Password,
    /// The file holds no private key with a certificate.
    NoKey,
    /// A key type magic-pdf cannot sign with (only RSA and ECDSA P-256/P-384).
    UnsupportedKey,
}

impl SigningIdentity {
    /// Reads a .p12/.pfx certificate file.
    pub fn from_pkcs12(data: &[u8], password: &str) -> Result<SigningIdentity, IdentityError> {
        use p12_keystore::{KeyStore, Pkcs12ImportPolicy};
        let store =
            KeyStore::from_pkcs12(data, password, Pkcs12ImportPolicy::Strict).map_err(|_| IdentityError::Password)?;
        let (_, chain) = store.private_key_chain().ok_or(IdentityError::NoKey)?;
        let certs: Vec<Vec<u8>> = chain.certs().iter().map(|c| c.as_der().to_vec()).collect();
        if certs.is_empty() {
            return Err(IdentityError::NoKey);
        }
        let key = parse_key(chain.key().as_der()).ok_or(IdentityError::UnsupportedKey)?;
        SigningIdentity::new(key, certs).ok_or(IdentityError::NoKey)
    }

    /// A certificate chain (signer first) whose key signs by itself.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn from_external(chain: Vec<Vec<u8>>, key: Box<dyn ExternalKey>) -> Option<SigningIdentity> {
        SigningIdentity::new(Key::External(key), chain)
    }

    fn kind(&self) -> KeyKind {
        match &self.key {
            Key::Rsa(_) => KeyKind::Rsa,
            Key::P256(_) => KeyKind::P256,
            Key::P384(_) => KeyKind::P384,
            Key::External(k) => k.kind(),
        }
    }

    fn new(key: Key, chain: Vec<Vec<u8>>) -> Option<SigningIdentity> {
        let (tlv, _) = Tlv::read(chain.first()?)?;
        let cert = Certificate::parse(&tlv)?;
        let (name, issuer, self_signed) =
            (cert.subject_name.clone(), cert.issuer_name.clone(), cert.subject_raw == cert.issuer_raw);
        let summary = certificate_summary(chain.first()?)?;
        let (expires, valid_now, for_signing) = (summary.expires, summary.valid_now, summary.for_signing);
        Some(SigningIdentity { key, chain, name, issuer, self_signed, expires, valid_now, for_signing })
    }

    fn digest_oid(&self) -> &'static [u8] {
        match self.kind() {
            KeyKind::P384 => OID_SHA384,
            _ => OID_SHA256,
        }
    }

    fn digest(&self, data: &[u8]) -> Vec<u8> {
        match self.kind() {
            KeyKind::P384 => Sha384::digest(data).to_vec(),
            _ => Sha256::digest(data).to_vec(),
        }
    }

    /// AlgorithmIdentifier of the signature.
    fn signature_algorithm(&self) -> Vec<u8> {
        match self.kind() {
            KeyKind::Rsa => seq(&[oid(OID_RSA), NULL.to_vec()]),
            KeyKind::P256 => seq(&[oid(OID_ECDSA_SHA256)]),
            KeyKind::P384 => seq(&[oid(OID_ECDSA_SHA384)]),
        }
    }

    /// Signs `data` (hashing it with the key's digest).
    fn sign(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        use rsa::signature::{SignatureEncoding, Signer};
        Ok(match &self.key {
            Key::Rsa(k) => k.sign(data).to_vec(),
            Key::P256(k) => {
                let s: p256::ecdsa::Signature = k.sign(data);
                s.to_der().as_bytes().to_vec()
            }
            Key::P384(k) => {
                let s: p384::ecdsa::Signature = k.sign(data);
                s.to_der().as_bytes().to_vec()
            }
            Key::External(k) => {
                let raw = k.sign_digest(&self.digest(data))?;
                match k.kind() {
                    KeyKind::Rsa => raw,
                    // CMS wants ECDSA signatures as SEQUENCE { r INTEGER, s INTEGER }.
                    _ => {
                        let (r, s) = raw.split_at(raw.len() / 2);
                        seq(&[der(INTEGER, &unsigned_integer(r)), der(INTEGER, &unsigned_integer(s))])
                    }
                }
            }
        })
    }
}

/// Name, issuer and expiry date (YYYY-MM-DD) of a certificate (DER).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn certificate_summary(der: &[u8]) -> Option<CertificateSummary> {
    let (tlv, _) = Tlv::read(der)?;
    let c = Certificate::parse(&tlv)?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    Some(CertificateSummary {
        name: c.subject_name.clone(),
        issuer: c.issuer_name.clone(),
        expires: c.not_after.clone(),
        // YYYY-MM-DD compares as text; an unreadable date counts as valid.
        valid_now: (c.not_before.is_empty() || c.not_before <= today)
            && (c.not_after.is_empty() || today <= c.not_after),
        for_signing: c.for_signing,
    })
}

/// What a certificate says about itself.
#[derive(Clone, Debug, PartialEq)]
pub struct CertificateSummary {
    pub name: String,
    pub issuer: String,
    /// Last day of validity (YYYY-MM-DD).
    pub expires: String,
    /// Today lies within its validity.
    pub valid_now: bool,
    /// Meant for signing (not e.g. a login-only device certificate).
    pub for_signing: bool,
}

/// The kind of key a certificate (DER) holds, if magic-pdf can sign with it.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn certificate_key_kind(der: &[u8]) -> Option<KeyKind> {
    use super::signatures::{Curve, Key as PublicKey};
    let (tlv, _) = Tlv::read(der)?;
    match Certificate::parse(&tlv)?.key {
        PublicKey::Rsa { .. } => Some(KeyKind::Rsa),
        PublicKey::Ec { curve: Curve::P256, .. } => Some(KeyKind::P256),
        PublicKey::Ec { curve: Curve::P384, .. } => Some(KeyKind::P384),
        PublicKey::Other => None,
    }
}

/// A PKCS#8 private key: RSA, or ECDSA on P-256 or P-384.
fn parse_key(der: &[u8]) -> Option<Key> {
    use rsa::pkcs8::DecodePrivateKey;
    if let Ok(k) = rsa::pkcs1v15::SigningKey::<Sha256>::from_pkcs8_der(der) {
        return Some(Key::Rsa(Box::new(k)));
    }
    if let Ok(k) = p256::ecdsa::SigningKey::from_pkcs8_der(der) {
        return Some(Key::P256(k));
    }
    p384::ecdsa::SigningKey::from_pkcs8_der(der).ok().map(Key::P384)
}

/// The mark in the signature box, next to the text.
#[derive(Clone, Debug, PartialEq)]
pub enum SignatureMark {
    None,
    /// Strokes drawn with the mouse, in 0..1 × 0..aspect (y down).
    Drawn {
        strokes: Vec<Vec<[f32; 2]>>,
        color: [u8; 3],
    },
    /// A picture (RGBA pixels).
    Image {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
}

#[derive(Clone, Debug)]
pub struct SignRequest {
    pub page: usize,
    /// The box on the page in PDF user space: left, bottom, right, top.
    pub rect: [f32; 4],
    pub mark: SignatureMark,
    /// Lines of text in the box (e.g. "Digitally signed by …", the date).
    pub lines: Vec<String>,
    pub reason: String,
    pub location: String,
    /// The signing time for the PDF (`D:…`).
    pub date: String,
    /// RFC 3161 timestamp server, if a trusted timestamp is wanted.
    pub timestamp_url: Option<String>,
}

/// Fetches a timestamp token: sends the request (DER) and returns the response (DER).
pub type TimestampFn<'a> = &'a dyn Fn(&str, &[u8]) -> Result<Vec<u8>, String>;

/// Signs the PDF `file` and returns the signed file (the original bytes plus an
/// incremental update).
pub fn sign(file: &[u8], req: &SignRequest, id: &SigningIdentity, timestamp: TimestampFn) -> Result<Vec<u8>, String> {
    let doc = Document::load_mem(file).map_err(|e| tf("could not read the PDF structure: {e}", &[("e", &e)]))?;
    if doc.is_encrypted() || doc.trailer.has(b"Encrypt") {
        return Err(t("encrypted PDFs cannot be signed").into());
    }
    let chain_len: usize = id.chain.iter().map(Vec::len).sum();
    // Room for the CMS structure; doubled if it turns out too small.
    let mut space = chain_len + 3000 + if req.timestamp_url.is_some() { 12_000 } else { 0 };
    for _ in 0..3 {
        let (mut out, contents_at, range_at) = prepare(file, doc.clone(), req, space)?;
        let hex_len = space * 2 + 2; // with the angle brackets
        let after = contents_at + hex_len;
        let ranges = [0, contents_at, after, out.len() - after];
        let mut range_text = format!("[{} {} {} {}]", ranges[0], ranges[1], ranges[2], ranges[3]);
        if range_text.len() > RANGE_PLACEHOLDER.len() {
            return Err(t("the file is too large to sign").into());
        }
        while range_text.len() < RANGE_PLACEHOLDER.len() {
            range_text.push(' ');
        }
        out[range_at..range_at + RANGE_PLACEHOLDER.len()].copy_from_slice(range_text.as_bytes());

        let mut signed = out[..contents_at].to_vec();
        signed.extend_from_slice(&out[after..]);
        let cms = cms(id, &id.digest(&signed), req, timestamp)?;
        if cms.len() > space {
            space = cms.len() + 2000;
            continue;
        }
        let mut hex = String::with_capacity(space * 2);
        for b in &cms {
            let _ = write!(hex, "{b:02X}");
        }
        out[contents_at + 1..contents_at + 1 + hex.len()].copy_from_slice(hex.as_bytes());
        return Ok(out);
    }
    Err(t("the signature does not fit").into())
}

const RANGE_PLACEHOLDER: &str = "[0 1111111111 2222222222 3333333333]";

/// Writes the incremental update with placeholders; returns the bytes, the offset
/// of the `<` that starts /Contents and the offset of the /ByteRange array.
fn prepare(file: &[u8], doc: Document, req: &SignRequest, space: usize) -> Result<(Vec<u8>, usize, usize), String> {
    let pages = doc.get_pages();
    let page_id = *pages.get(&(req.page as u32 + 1)).ok_or(t("the page does not exist"))?;
    let rotation = doc
        .get_object(page_id)
        .and_then(Object::as_dict)
        .ok()
        .and_then(|p| p.get(b"Rotate").ok())
        .and_then(|r| r.as_i64().ok())
        .unwrap_or(0)
        .rem_euclid(360);
    let root_id = doc.trailer.get(b"Root").and_then(Object::as_reference).map_err(|_| t("the PDF has no catalog"))?;
    let field_name = unique_field_name(&doc);
    let mut inc = IncrementalDocument::create_from(file.to_vec(), doc);

    // The signature value: placeholders for the byte ranges and the CMS data.
    let mut sig = Dictionary::new();
    sig.set("Type", Object::Name(b"Sig".to_vec()));
    sig.set("Filter", Object::Name(b"Adobe.PPKLite".to_vec()));
    sig.set("SubFilter", Object::Name(b"ETSI.CAdES.detached".to_vec()));
    // Wide numbers, filled in once the offsets are known (same width, so nothing moves).
    let placeholder = [0, 1_111_111_111, 2_222_222_222, 3_333_333_333];
    sig.set("ByteRange", Object::Array(placeholder.iter().map(|&v| Object::Integer(v)).collect()));
    sig.set("Contents", Object::String(vec![0; space], StringFormat::Hexadecimal));
    sig.set("M", Object::string_literal(req.date.clone()));
    if !req.reason.is_empty() {
        sig.set("Reason", text_string(&req.reason));
    }
    if !req.location.is_empty() {
        sig.set("Location", text_string(&req.location));
    }
    let sig_id = inc.new_document.add_object(sig);

    // The field and its widget (one dictionary), with the visible appearance.
    let [l, b, r, top] = req.rect;
    let appearance = appearance(&mut inc.new_document, r - l, top - b, rotation, req);
    let mut widget = Dictionary::new();
    widget.set("Type", Object::Name(b"Annot".to_vec()));
    widget.set("Subtype", Object::Name(b"Widget".to_vec()));
    widget.set("FT", Object::Name(b"Sig".to_vec()));
    widget.set("T", text_string(&field_name));
    widget.set("Rect", Object::Array(req.rect.iter().map(|&v| Object::Real(v)).collect()));
    widget.set("F", Object::Integer(132)); // print, locked
    widget.set("P", Object::Reference(page_id));
    widget.set("V", Object::Reference(sig_id));
    let mut ap = Dictionary::new();
    ap.set("N", Object::Reference(appearance));
    widget.set("AP", Object::Dictionary(ap));
    let widget_id = inc.new_document.add_object(widget);

    // The page lists the widget.
    inc.opt_clone_object_to_new_document(page_id).map_err(|e| e.to_string())?;
    let annots = inc.new_document.get_dictionary(page_id).ok().and_then(|p| p.get(b"Annots").ok().cloned());
    match annots {
        Some(Object::Reference(id)) => {
            inc.opt_clone_object_to_new_document(id).map_err(|e| e.to_string())?;
            push_ref(inc.new_document.get_object_mut(id).map_err(|e| e.to_string())?, widget_id)?;
        }
        Some(Object::Array(_)) => {
            let page = inc.new_document.get_dictionary_mut(page_id).map_err(|e| e.to_string())?;
            push_ref(page.get_mut(b"Annots").map_err(|e| e.to_string())?, widget_id)?;
        }
        _ => {
            let page = inc.new_document.get_dictionary_mut(page_id).map_err(|e| e.to_string())?;
            page.set("Annots", Object::Array(vec![Object::Reference(widget_id)]));
        }
    }

    // The form lists the field; SigFlags 3: signatures exist, append only.
    inc.opt_clone_object_to_new_document(root_id).map_err(|e| e.to_string())?;
    let acroform = inc.new_document.get_dictionary(root_id).ok().and_then(|c| c.get(b"AcroForm").ok().cloned());
    let form_id = match acroform {
        Some(Object::Reference(id)) => {
            inc.opt_clone_object_to_new_document(id).map_err(|e| e.to_string())?;
            Some(id)
        }
        Some(Object::Dictionary(_)) => None,
        _ => {
            let catalog = inc.new_document.get_dictionary_mut(root_id).map_err(|e| e.to_string())?;
            catalog.set("AcroForm", Object::Dictionary(Dictionary::new()));
            None
        }
    };
    let fields = {
        let form = match form_id {
            Some(id) => inc.new_document.get_dictionary_mut(id),
            None => inc.new_document.get_dictionary_mut(root_id).and_then(|c| c.get_mut(b"AcroForm")?.as_dict_mut()),
        }
        .map_err(|e| e.to_string())?;
        form.set("SigFlags", Object::Integer(3));
        match form.get(b"Fields").ok().cloned() {
            Some(Object::Reference(id)) => Some(id),
            Some(Object::Array(_)) => {
                push_ref(form.get_mut(b"Fields").map_err(|e| e.to_string())?, widget_id)?;
                None
            }
            _ => {
                form.set("Fields", Object::Array(vec![Object::Reference(widget_id)]));
                None
            }
        }
    };
    if let Some(id) = fields {
        inc.opt_clone_object_to_new_document(id).map_err(|e| e.to_string())?;
        push_ref(inc.new_document.get_object_mut(id).map_err(|e| e.to_string())?, widget_id)?;
    }

    let mut out = Vec::new();
    inc.save_to(&mut out).map_err(|e| tf("could not write the PDF: {e}", &[("e", &e)]))?;

    let update_start = file.len();
    let find =
        |needle: &[u8]| out[update_start..].windows(needle.len()).position(|w| w == needle).map(|p| p + update_start);
    let range_at =
        find(RANGE_PLACEHOLDER.as_bytes()).ok_or(t("the signature dictionary was not written as expected"))?;
    let contents_at = find(&[b"<".as_slice(), &vec![b'0'; space * 2], b">"].concat())
        .ok_or(t("the signature dictionary was not written as expected"))?;
    Ok((out, contents_at, range_at))
}

fn push_ref(array: &mut Object, id: ObjectId) -> Result<(), String> {
    array.as_array_mut().map_err(|e| e.to_string())?.push(Object::Reference(id));
    Ok(())
}

/// "Signature1", "Signature2", … not yet used by a field.
fn unique_field_name(doc: &Document) -> String {
    let taken: Vec<Vec<u8>> = doc
        .objects
        .values()
        .filter_map(|o| o.as_dict().ok())
        .filter_map(|d| d.get(b"T").and_then(Object::as_str).ok().map(<[u8]>::to_vec))
        .collect();
    (1..).map(|n| format!("Signature{n}")).find(|n| !taken.iter().any(|t| t == n.as_bytes())).unwrap_or_default()
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

/// The visible appearance: the mark on the left (or on top in a tall box) and the
/// lines of text beside it. `w` × `h` is the box on the unrotated page.
fn appearance(doc: &mut Document, w: f32, h: f32, rotation: i64, req: &SignRequest) -> ObjectId {
    // Drawn upright as the page is shown.
    let (bw, bh) = if rotation % 180 == 90 { (h, w) } else { (w, h) };
    let matrix = match rotation {
        90 => [0.0, 1.0, -1.0, 0.0, bh, 0.0],
        180 => [-1.0, 0.0, 0.0, -1.0, bw, bh],
        270 => [0.0, -1.0, 1.0, 0.0, 0.0, bw],
        _ => [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
    };
    let pad = (bh.min(bw) * 0.06).clamp(1.0, 6.0);
    let has_mark = req.mark != SignatureMark::None;
    let side_by_side = bw >= bh * 1.6;
    // (x, y, w, h) of the mark and of the text.
    let (mark_box, text_box) = match (has_mark, side_by_side) {
        (false, _) => ([0.0; 4], [pad, pad, bw - 2.0 * pad, bh - 2.0 * pad]),
        (true, true) => {
            let mw = bw * 0.5 - 1.5 * pad;
            ([pad, pad, mw, bh - 2.0 * pad], [bw * 0.5 + 0.5 * pad, pad, mw, bh - 2.0 * pad])
        }
        (true, false) => {
            let th = (bh - 3.0 * pad) * 0.4;
            ([pad, 2.0 * pad + th, bw - 2.0 * pad, bh - 3.0 * pad - th], [pad, pad, bw - 2.0 * pad, th])
        }
    };

    let mut content = String::new();
    let mut resources = Dictionary::new();
    match &req.mark {
        SignatureMark::None => {}
        SignatureMark::Drawn { strokes, color } => {
            let (mut max_x, mut max_y) = (0.0f32, 0.0f32);
            for p in strokes.iter().flatten() {
                max_x = max_x.max(p[0]);
                max_y = max_y.max(p[1]);
            }
            let (sw, sh) = (max_x.max(1e-3), max_y.max(1e-3));
            let scale = (mark_box[2] / sw).min(mark_box[3] / sh);
            let ox = mark_box[0] + (mark_box[2] - sw * scale) / 2.0;
            let oy = mark_box[1] + (mark_box[3] + sh * scale) / 2.0; // top of the drawing
            let [r, g, b] = color.map(|c| c as f32 / 255.0);
            let _ = writeln!(content, "q {r:.3} {g:.3} {b:.3} RG {:.2} w 1 J 1 j", (scale * 0.012).clamp(0.6, 3.0));
            for stroke in strokes {
                for (i, p) in stroke.iter().enumerate() {
                    let (x, y) = (ox + p[0] * scale, oy - p[1] * scale);
                    let _ = writeln!(content, "{x:.2} {y:.2} {}", if i == 0 { "m" } else { "l" });
                }
                if stroke.len() == 1 {
                    let (x, y) = (ox + stroke[0][0] * scale, oy - stroke[0][1] * scale);
                    let _ = writeln!(content, "{:.2} {y:.2} l", x + 0.1);
                }
                content.push_str("S\n");
            }
            content.push_str("Q\n");
        }
        SignatureMark::Image { width, height, rgba } => {
            let image = image_xobject(doc, *width, *height, rgba);
            let mut xobjects = Dictionary::new();
            xobjects.set("Im1", Object::Reference(image));
            resources.set("XObject", Object::Dictionary(xobjects));
            let (iw, ih) = (*width as f32, *height as f32);
            let scale = (mark_box[2] / iw).min(mark_box[3] / ih);
            let (dw, dh) = (iw * scale, ih * scale);
            let x = mark_box[0] + (mark_box[2] - dw) / 2.0;
            let y = mark_box[1] + (mark_box[3] - dh) / 2.0;
            let _ = writeln!(content, "q {dw:.2} 0 0 {dh:.2} {x:.2} {y:.2} cm /Im1 Do Q");
        }
    }
    if !req.lines.is_empty() {
        let mut font = Dictionary::new();
        font.set("Type", Object::Name(b"Font".to_vec()));
        font.set("Subtype", Object::Name(b"Type1".to_vec()));
        font.set("BaseFont", Object::Name(b"Helvetica".to_vec()));
        font.set("Encoding", Object::Name(b"WinAnsiEncoding".to_vec()));
        let mut fonts = Dictionary::new();
        fonts.set("F1", Object::Dictionary(font));
        resources.set("Font", Object::Dictionary(fonts));
        let n = req.lines.len() as f32;
        let widest = req.lines.iter().map(|l| helvetica_width(l)).fold(0.0f32, f32::max).max(1.0);
        let size = (text_box[3] / (n * 1.25)).min(text_box[2] / widest).clamp(2.0, 10.0);
        let leading = size * 1.25;
        let top = text_box[1] + (text_box[3] + n * leading) / 2.0 - size;
        let _ = writeln!(content, "BT /F1 {size:.2} Tf 0 0 0 rg {leading:.2} TL {:.2} {top:.2} Td", text_box[0]);
        for (i, line) in req.lines.iter().enumerate() {
            if i > 0 {
                content.push_str("T*\n");
            }
            content.push('(');
            for b in win_ansi(line) {
                match b {
                    b'(' | b')' | b'\\' => {
                        content.push('\\');
                        content.push(b as char);
                    }
                    0x20..=0x7E => content.push(b as char),
                    _ => {
                        let _ = write!(content, "\\{b:03o}");
                    }
                }
            }
            content.push_str(") Tj\n");
        }
        content.push_str("ET\n");
    }

    let mut dict = Dictionary::new();
    dict.set("Type", Object::Name(b"XObject".to_vec()));
    dict.set("Subtype", Object::Name(b"Form".to_vec()));
    dict.set("BBox", Object::Array(vec![0.into(), 0.into(), Object::Real(bw), Object::Real(bh)]));
    dict.set("Matrix", Object::Array(matrix.iter().map(|&v| Object::Real(v)).collect()));
    dict.set("Resources", Object::Dictionary(resources));
    let mut stream = Stream::new(dict, content.into_bytes());
    let _ = stream.compress();
    doc.add_object(stream)
}

/// An RGB image with its alpha channel as a soft mask.
fn image_xobject(doc: &mut Document, width: u32, height: u32, rgba: &[u8]) -> ObjectId {
    let rgb: Vec<u8> = rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
    let alpha: Vec<u8> = rgba.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
    let image = |color_space: &[u8], data: Vec<u8>| {
        let mut d = Dictionary::new();
        d.set("Type", Object::Name(b"XObject".to_vec()));
        d.set("Subtype", Object::Name(b"Image".to_vec()));
        d.set("Width", Object::Integer(width as i64));
        d.set("Height", Object::Integer(height as i64));
        d.set("ColorSpace", Object::Name(color_space.to_vec()));
        d.set("BitsPerComponent", Object::Integer(8));
        let mut s = Stream::new(d, data);
        let _ = s.compress();
        s
    };
    let mut main = image(b"DeviceRGB", rgb);
    if alpha.iter().any(|&a| a < 255) {
        let mask = doc.add_object(image(b"DeviceGray", alpha));
        main.dict.set("SMask", Object::Reference(mask));
    }
    doc.add_object(main)
}

/// Text in WinAnsiEncoding (the Latin-1 range; other characters become "?").
fn win_ansi(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| match c {
            '–' => 0x96,
            '—' => 0x97,
            '‘' => 0x91,
            '’' => 0x92,
            '“' => 0x93,
            '”' => 0x94,
            '…' => 0x85,
            '€' => 0x80,
            c if (c as u32) < 0x80 || (0xA0..=0xFF).contains(&(c as u32)) => c as u8,
            _ => b'?',
        })
        .collect()
}

/// Approximate width of a line of Helvetica at size 1.
fn helvetica_width(s: &str) -> f32 {
    s.chars()
        .map(|c| match c {
            'i' | 'j' | 'l' | '.' | ',' | ':' | ';' | '\'' | '|' | '!' => 0.25,
            ' ' | 'f' | 't' | 'r' | '(' | ')' | '-' | '/' | 'I' => 0.32,
            'm' | 'w' | 'M' | 'W' => 0.85,
            c if c.is_uppercase() || c.is_ascii_digit() => 0.65,
            _ => 0.55,
        })
        .sum()
}

/// The CMS SignedData (in a ContentInfo) over the document digest.
fn cms(id: &SigningIdentity, digest: &[u8], req: &SignRequest, timestamp: TimestampFn) -> Result<Vec<u8>, String> {
    let digest_alg = seq(&[oid(id.digest_oid())]);
    let cert_hash = Sha256::digest(&id.chain[0]).to_vec();
    // contentType, messageDigest, signingCertificateV2 (ESS, as PAdES asks).
    let attrs = vec![
        attribute(OID_CONTENT_TYPE, &oid(OID_DATA)),
        attribute(OID_MESSAGE_DIGEST, &octets(digest)),
        attribute(OID_SIGNING_CERT_V2, &seq(&[seq(&[seq(&[octets(&cert_hash)])])])),
    ];
    let signed_attrs = set_of(attrs);
    let signature = id.sign(&signed_attrs)?;

    let (tlv, _) = Tlv::read(&id.chain[0]).ok_or(t("the certificate cannot be read"))?;
    let cert = Certificate::parse(&tlv).ok_or(t("the certificate cannot be read"))?;
    let sid = seq(&[cert.issuer_raw.to_vec(), der(INTEGER, cert.serial)]);
    let mut signer_info =
        vec![der(INTEGER, &[1]), sid, digest_alg.clone(), implicit(0, &signed_attrs), id.signature_algorithm()];
    signer_info.push(octets(&signature));
    if let Some(url) = &req.timestamp_url {
        let token = timestamp_token(url, &signature, timestamp)?;
        signer_info.push(implicit(1, &set_of(vec![seq(&[oid(OID_TIMESTAMP_TOKEN), set_of(vec![token])])])));
    }
    let certs: Vec<u8> = id.chain.concat();
    let signed_data = seq(&[
        der(INTEGER, &[1]),
        set_of(vec![digest_alg]),
        seq(&[oid(OID_DATA)]),
        der(0xA0, &certs),
        set_of(vec![seq(&signer_info)]),
    ]);
    Ok(seq(&[oid(OID_SIGNED_DATA), der(0xA0, &signed_data)]))
}

/// Asks the timestamp server for a token over the signature value.
fn timestamp_token(url: &str, signature: &[u8], fetch: TimestampFn) -> Result<Vec<u8>, String> {
    let nonce = {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let mut n = Sha256::digest([signature, &t.to_be_bytes()].concat())[..8].to_vec();
        n[0] &= 0x7F; // positive
        n
    };
    let request = seq(&[
        der(INTEGER, &[1]),
        seq(&[seq(&[oid(OID_SHA256), NULL.to_vec()]), octets(&Sha256::digest(signature))]),
        der(INTEGER, &nonce),
        der(0x01, &[0xFF]), // certReq: the server's certificate in the token
    ]);
    let response = fetch(url, &request)?;
    // TimeStampResp { status PKIStatusInfo { status INTEGER, ... }, timeStampToken }
    let (resp, _) = Tlv::read(&response).ok_or(t("the timestamp server sent an unreadable answer"))?;
    let mut parts = resp.children();
    let status =
        parts.next().and_then(|s| s.children().next()).ok_or(t("the timestamp server sent an unreadable answer"))?;
    if !matches!(status.value, [0] | [1]) {
        return Err(t("the timestamp server refused the request").into());
    }
    let token = parts.next().ok_or(t("the timestamp server sent no timestamp"))?;
    // The token must be over this request: compare the nonce.
    if !contains(token.raw, &der(INTEGER, &nonce)) {
        return Err(t("the timestamp does not match the request").into());
    }
    Ok(token.raw.to_vec())
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

// --- A minimal DER writer ---

const INTEGER: u8 = 0x02;
const NULL: &[u8] = &[0x05, 0x00];

fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes: Vec<u8> = len.to_be_bytes().into_iter().skip_while(|&b| b == 0).collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
    out.extend_from_slice(content);
    out
}

/// The contents of a DER INTEGER for an unsigned big-endian number.
fn unsigned_integer(n: &[u8]) -> Vec<u8> {
    let trimmed = match n.iter().position(|&b| b != 0) {
        Some(i) => &n[i..],
        None => &[0][..],
    };
    let mut out = Vec::with_capacity(trimmed.len() + 1);
    if trimmed[0] & 0x80 != 0 {
        out.push(0);
    }
    out.extend_from_slice(trimmed);
    out
}

fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    der(0x30, &parts.concat())
}

/// A DER SET OF: the elements sorted by their encoding.
fn set_of(mut parts: Vec<Vec<u8>>) -> Vec<u8> {
    parts.sort();
    der(0x31, &parts.concat())
}

/// `[n] IMPLICIT` for a constructed element (replaces its tag).
fn implicit(n: u8, element: &[u8]) -> Vec<u8> {
    let mut out = element.to_vec();
    out[0] = 0xA0 | n;
    out
}

fn oid(value: &[u8]) -> Vec<u8> {
    der(0x06, value)
}

fn octets(value: &[u8]) -> Vec<u8> {
    der(0x04, value)
}

fn attribute(kind: &[u8], value: &[u8]) -> Vec<u8> {
    seq(&[oid(kind), set_of(vec![value.to_vec()])])
}

const OID_DATA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x01];
const OID_SIGNED_DATA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x02];
const OID_CONTENT_TYPE: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x03];
const OID_MESSAGE_DIGEST: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x04];
const OID_SIGNING_CERT_V2: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x10, 0x02, 0x2F];
const OID_TIMESTAMP_TOKEN: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x10, 0x02, 0x0E];
const OID_SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
const OID_SHA384: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02];
const OID_RSA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];
const OID_ECDSA_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
const OID_ECDSA_SHA384: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x03];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::signatures::{SigStatus, check};

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
    }

    fn request(page: usize) -> SignRequest {
        SignRequest {
            page,
            rect: [100.0, 100.0, 300.0, 160.0],
            mark: SignatureMark::Drawn { strokes: vec![vec![[0.0, 0.2], [0.5, 0.0], [1.0, 0.3]]], color: [0, 0, 128] },
            lines: vec!["Digitally signed by Test".into(), "2026-11-27 10:00".into()],
            reason: "Approved".into(),
            location: "Zürich".into(),
            date: "D:20261127100000Z".into(),
            timestamp_url: None,
        }
    }

    /// Checks every signature in `file` the way the viewer does.
    fn verify_all(file: &[u8]) -> Vec<(String, SigStatus)> {
        let doc = Document::load_mem(file).unwrap();
        let mut out = Vec::new();
        for obj in doc.objects.values() {
            let Ok(d) = obj.as_dict() else { continue };
            if d.get(b"Type").and_then(Object::as_name).ok() != Some(b"Sig".as_slice()) {
                continue;
            }
            let r: Vec<usize> =
                d.get(b"ByteRange").unwrap().as_array().unwrap().iter().map(|o| o.as_i64().unwrap() as usize).collect();
            let contents = d.get(b"Contents").unwrap().as_str().unwrap();
            let sub = String::from_utf8_lossy(d.get(b"SubFilter").unwrap().as_name().unwrap()).into_owned();
            let c = check(file, &[(r[0], r[1]), (r[2], r[3])], contents, &sub);
            out.push((c.signer, c.status));
        }
        out
    }

    #[test]
    fn signs_with_rsa_ecdsa_and_legacy_certificate_files() {
        assert_eq!(
            SigningIdentity::from_pkcs12(&fixture("signer-rsa.p12"), "wrong").unwrap_err(),
            IdentityError::Password
        );
        let sample = fixture("sample.pdf");
        for (file, name) in [
            ("signer-rsa.p12", "Test Signer RSA"),
            ("signer-ec.p12", "Test Signer EC"),
            ("signer-legacy.p12", "Test Signer RSA"),
        ] {
            let id = SigningIdentity::from_pkcs12(&fixture(file), "magic").unwrap();
            assert_eq!(id.name, name);
            assert!(id.self_signed);
            let signed = sign(&sample, &request(1), &id, &|_, _| Err("offline".into())).unwrap();
            assert!(signed.starts_with(&sample), "{file}: an incremental update");
            assert_eq!(verify_all(&signed), vec![(name.to_string(), SigStatus::Valid)], "{file}");
            // Any change to the signed bytes is noticed.
            let mut changed = signed.clone();
            let at = sample.len() / 2;
            changed[at] ^= 1;
            assert!(matches!(verify_all(&changed)[0].1, SigStatus::Invalid(_)), "{file}");
        }
    }

    #[test]
    fn a_second_signature_keeps_the_first_one_valid() {
        let id = SigningIdentity::from_pkcs12(&fixture("signer-ec.p12"), "magic").unwrap();
        let first = sign(&fixture("signed.pdf"), &request(0), &id, &|_, _| Err("offline".into())).unwrap();
        let mut sigs = verify_all(&first);
        sigs.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(sigs.len(), 2);
        assert_eq!(sigs[0], ("Test Signer EC".to_string(), SigStatus::Valid));
        assert_eq!(sigs[1].1, SigStatus::ChangedAfter, "the earlier signature: changed after, still intact");
        // Field names stay unique.
        let doc = Document::load_mem(&first).unwrap();
        let names: Vec<Vec<u8>> = doc
            .objects
            .values()
            .filter_map(|o| o.as_dict().ok()?.get(b"T").ok()?.as_str().ok().map(<[u8]>::to_vec))
            .collect();
        assert!(names.contains(&b"Signature2".to_vec()), "{names:?}");
    }

    /// An external key (as the Windows store provides) returning raw ECDSA r‖s and
    /// RSA signatures over digests.
    #[test]
    fn external_keys_sign_digests() {
        struct Ec(p256::ecdsa::SigningKey);
        impl ExternalKey for Ec {
            fn kind(&self) -> KeyKind {
                KeyKind::P256
            }
            fn sign_digest(&self, digest: &[u8]) -> Result<Vec<u8>, String> {
                use p256::ecdsa::signature::hazmat::PrehashSigner;
                let s: p256::ecdsa::Signature = self.0.sign_prehash(digest).map_err(|e| e.to_string())?;
                Ok(s.to_bytes().to_vec())
            }
        }
        let p12 = p12_keystore::KeyStore::from_pkcs12(
            &fixture("signer-ec.p12"),
            "magic",
            p12_keystore::Pkcs12ImportPolicy::Strict,
        )
        .unwrap();
        let (_, chain) = p12.private_key_chain().unwrap();
        use rsa::pkcs8::DecodePrivateKey;
        let key = p256::ecdsa::SigningKey::from_pkcs8_der(chain.key().as_der()).unwrap();
        let certs = chain.certs().iter().map(|c| c.as_der().to_vec()).collect();
        let id = SigningIdentity::from_external(certs, Box::new(Ec(key))).unwrap();
        assert_eq!(id.name, "Test Signer EC");
        assert_eq!(id.expires.len(), 10, "{}", id.expires);
        let signed = sign(&fixture("sample.pdf"), &request(0), &id, &|_, _| Err("offline".into())).unwrap();
        assert_eq!(verify_all(&signed), vec![("Test Signer EC".to_string(), SigStatus::Valid)]);
        assert_eq!(unsigned_integer(&[0, 0, 0x80, 1]), vec![0, 0x80, 1]);
        assert_eq!(unsigned_integer(&[0, 0x7F]), vec![0x7F]);
    }

    /// Which certificates the signing dialog offers by default.
    #[test]
    fn certificates_for_signing_are_told_apart() {
        let summary = |name: &str| certificate_summary(&fixture(name)).unwrap();
        let login = summary("cert-login.der");
        assert!(login.valid_now && !login.for_signing, "a login-only certificate: {login:?}");
        let email = summary("cert-email.der");
        assert!(email.valid_now && email.for_signing, "e-mail protection and signatures: {email:?}");
        let old = summary("cert-expired.der");
        assert!(!old.valid_now && old.for_signing, "expired: {old:?}");
        assert_eq!(old.expires, "2021-01-01");
        // The test certificates without stated purposes may sign.
        let id = SigningIdentity::from_pkcs12(&fixture("signer-rsa.p12"), "magic").unwrap();
        assert!(id.valid_now && id.for_signing);
    }

    #[test]
    fn a_failed_timestamp_stops_signing() {
        let id = SigningIdentity::from_pkcs12(&fixture("signer-rsa.p12"), "magic").unwrap();
        let mut req = request(0);
        req.timestamp_url = Some("http://tsa.invalid".into());
        let err = sign(&fixture("sample.pdf"), &req, &id, &|_, _| Err("could not connect".into())).unwrap_err();
        assert!(err.contains("could not connect"), "{err}");
    }
}
