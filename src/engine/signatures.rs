//! Checking digital signatures of PDFs (detached CMS / PKCS#7 signatures, the kind
//! Acrobat and most signing tools write): that the signed bytes are unchanged, that
//! the signature matches the signer's certificate, and whether the file was changed
//! after signing. Whether the certificate itself is trusted (a chain up to a
//! certificate authority, revocation) is not checked.
//!
//! The CMS structures are read with a small DER reader below; only the RSA and ECDSA
//! maths come from the RustCrypto crates.

use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};

/// Outcome of checking one signature.
#[derive(Clone, Debug, PartialEq)]
pub enum SigStatus {
    /// The signed part is unchanged and the signature matches the certificate.
    Valid,
    /// As `Valid`, but the file has changes added after this signature.
    ChangedAfter,
    /// The signed part was changed, or the signature does not match (why, in English).
    Invalid(&'static str),
    /// The signature could not be checked (an unsupported format or algorithm).
    Unknown(&'static str),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SignatureCheck {
    pub status: SigStatus,
    /// Common name (or organisation) of the signer's certificate.
    pub signer: String,
    /// Common name of the certificate's issuer.
    pub issuer: String,
    /// The certificate was issued by the signer itself (anyone could have made it).
    pub self_signed: bool,
}

/// Checks a signature. `ranges` are the (offset, length) pairs of the signature's
/// /ByteRange in `file`; `contents` is the decoded /Contents.
pub fn check(file: &[u8], ranges: &[(usize, usize)], contents: &[u8], subfilter: &str) -> SignatureCheck {
    let mut out = SignatureCheck {
        status: SigStatus::Unknown(""),
        signer: String::new(),
        issuer: String::new(),
        self_signed: false,
    };
    out.status = match verify(file, ranges, contents, subfilter, &mut out) {
        Ok(status) => status,
        Err(Problem::Invalid(m)) => SigStatus::Invalid(m),
        Err(Problem::Unknown(m)) => SigStatus::Unknown(m),
    };
    out
}

enum Problem {
    Invalid(&'static str),
    Unknown(&'static str),
}

fn unknown(m: &'static str) -> Problem {
    Problem::Unknown(m)
}

fn verify(
    file: &[u8],
    ranges: &[(usize, usize)],
    contents: &[u8],
    subfilter: &str,
    out: &mut SignatureCheck,
) -> Result<SigStatus, Problem> {
    if !matches!(subfilter, "adbe.pkcs7.detached" | "ETSI.CAdES.detached") {
        return Err(unknown("this signature format is not supported"));
    }
    // The signed bytes.
    let mut data = Vec::new();
    for &(start, len) in ranges {
        let part = start
            .checked_add(len)
            .and_then(|end| file.get(start..end))
            .ok_or(Problem::Invalid("the signed range lies outside the file"))?;
        data.extend_from_slice(part);
    }
    let end = ranges.iter().map(|&(s, l)| s + l).max().unwrap_or(0);
    let covers_whole =
        ranges.first().is_some_and(|r| r.0 == 0) && file[end.min(file.len())..].iter().all(u8::is_ascii_whitespace);

    // ContentInfo { contentType signedData, [0] SignedData }
    let (content_info, _) = Tlv::read(contents).ok_or(unknown("the signature data cannot be read"))?;
    let mut ci = content_info.children();
    let oid = ci.next().filter(|t| t.tag == OID).ok_or(unknown("not a CMS signature"))?;
    if oid.value != OID_SIGNED_DATA {
        return Err(unknown("not a CMS signature"));
    }
    let explicit = ci.next().filter(|t| t.tag == 0xA0).ok_or(unknown("CMS signature without content"))?;
    let signed_data = explicit.children().next().ok_or(unknown("CMS signature without content"))?;
    // SignedData { version, digestAlgorithms, encapContentInfo, [0] certificates, [1] crls, signerInfos }
    let mut certs: Vec<Tlv> = Vec::new();
    let mut signer_infos = None;
    for (i, part) in signed_data.children().enumerate() {
        match part.tag {
            0xA0 => certs = part.children().filter(|c| c.tag == SEQUENCE).collect(),
            SET if i > 2 => signer_infos = Some(part),
            _ => {}
        }
    }
    let signer_info = signer_infos.and_then(|s| s.children().next()).ok_or(unknown("the signature has no signer"))?;
    // SignerInfo { version, sid, digestAlgorithm, [0] signedAttrs, signatureAlgorithm, signature, ... }
    let parts: Vec<Tlv> = signer_info.children().collect();
    let sid = parts.get(1).ok_or(unknown("the signer is not named"))?;
    let digest_oid = parts.get(2).and_then(|a| a.children().next()).ok_or(unknown("no digest algorithm"))?;
    let hash = HashAlg::from_oid(digest_oid.value).ok_or(unknown("the digest algorithm is not supported"))?;
    let signed_attrs = parts.iter().find(|p| p.tag == 0xA0);
    let after_attrs = parts.iter().skip(3).filter(|p| p.tag != 0xA0);
    let mut rest = after_attrs;
    let sig_alg = rest.next().and_then(|a| a.children().next()).ok_or(unknown("no signature algorithm"))?;
    let signature = rest.next().filter(|s| s.tag == OCTET_STRING).ok_or(unknown("no signature value"))?;

    // The signer's certificate: matched by issuer and serial number, or by key identifier.
    let cert = certs
        .iter()
        .filter_map(Certificate::parse)
        .find(|c| c.matches(sid))
        .ok_or(unknown("the signer's certificate is not included"))?;
    out.signer = cert.subject_name.clone();
    out.issuer = cert.issuer_name.clone();
    out.self_signed = cert.subject_raw == cert.issuer_raw;

    // What was signed: the signed attributes (which hold the document's digest), or
    // the document itself.
    let digest = hash.digest(&data);
    let message_hash = match signed_attrs {
        Some(attrs) => {
            let message_digest = attrs
                .children()
                .find_map(|attr| {
                    let mut a = attr.children();
                    (a.next()?.value == OID_MESSAGE_DIGEST).then(|| a.next()?.children().next().map(|v| v.value))?
                })
                .ok_or(unknown("the signature does not record the document's digest"))?;
            if message_digest != digest.as_slice() {
                return Err(Problem::Invalid("the document was changed after it was signed"));
            }
            // Signed as a SET OF (tag 0x31), not with its [0] tag.
            let mut encoded = attrs.raw.to_vec();
            encoded[0] = SET;
            hash.digest(&encoded)
        }
        None => digest,
    };

    let ok = match (&cert.key, sig_alg.value) {
        (Key::Rsa { n, e }, oid) if oid == OID_RSA || HashAlg::from_rsa_oid(oid).is_some() => {
            verify_rsa(n, e, hash, &message_hash, signature.value)?
        }
        (Key::Ec { curve, point }, oid) if oid.starts_with(OID_ECDSA_PREFIX) || oid == OID_EC_PUBLIC_KEY => {
            verify_ecdsa(*curve, point, &message_hash, signature.value)?
        }
        (_, oid) if oid == OID_RSA_PSS => return Err(unknown("RSA-PSS signatures are not supported")),
        _ => return Err(unknown("the signature algorithm is not supported")),
    };
    if !ok {
        return Err(Problem::Invalid("the signature does not match the signer's certificate"));
    }
    Ok(if covers_whole { SigStatus::Valid } else { SigStatus::ChangedAfter })
}

fn verify_rsa(n: &[u8], e: &[u8], hash: HashAlg, hashed: &[u8], sig: &[u8]) -> Result<bool, Problem> {
    use rsa::{BoxedUint, Pkcs1v15Sign, PublicKey};
    let key = PublicKey::new(BoxedUint::from_be_slice_vartime(n), BoxedUint::from_be_slice_vartime(e))
        .map_err(|_| unknown("the RSA key is not usable"))?;
    let scheme = match hash {
        HashAlg::Sha1 => Pkcs1v15Sign::new::<Sha1>(),
        HashAlg::Sha256 => Pkcs1v15Sign::new::<Sha256>(),
        HashAlg::Sha384 => Pkcs1v15Sign::new::<Sha384>(),
        HashAlg::Sha512 => Pkcs1v15Sign::new::<Sha512>(),
    };
    Ok(key.verify(scheme, hashed, sig).is_ok())
}

fn verify_ecdsa(curve: Curve, point: &[u8], hashed: &[u8], sig: &[u8]) -> Result<bool, Problem> {
    // Ecdsa-Sig-Value ::= SEQUENCE { r INTEGER, s INTEGER } as fixed-size r || s.
    let size = match curve {
        Curve::P256 => 32,
        Curve::P384 => 48,
    };
    let (seq, _) = Tlv::read(sig).ok_or(Problem::Invalid("the signature value cannot be read"))?;
    let mut rs = Vec::with_capacity(size * 2);
    for int in seq.children().take(2) {
        let v = int.value.strip_prefix(&[0]).unwrap_or(int.value);
        if v.len() > size {
            return Ok(false);
        }
        rs.extend(std::iter::repeat_n(0, size - v.len()));
        rs.extend_from_slice(v);
    }
    if rs.len() != size * 2 {
        return Ok(false);
    }
    Ok(match curve {
        Curve::P256 => {
            use p256::ecdsa::{Signature, VerifyingKey, signature::hazmat::PrehashVerifier};
            let key = VerifyingKey::from_sec1_bytes(point).map_err(|_| unknown("the EC key is not usable"))?;
            Signature::from_slice(&rs).is_ok_and(|s| key.verify_prehash(hashed, &s).is_ok())
        }
        Curve::P384 => {
            use p384::ecdsa::{Signature, VerifyingKey, signature::hazmat::PrehashVerifier};
            let key = VerifyingKey::from_sec1_bytes(point).map_err(|_| unknown("the EC key is not usable"))?;
            Signature::from_slice(&rs).is_ok_and(|s| key.verify_prehash(hashed, &s).is_ok())
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum HashAlg {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl HashAlg {
    fn from_oid(oid: &[u8]) -> Option<HashAlg> {
        match oid {
            OID_SHA1 => Some(HashAlg::Sha1),
            OID_SHA256 => Some(HashAlg::Sha256),
            OID_SHA384 => Some(HashAlg::Sha384),
            OID_SHA512 => Some(HashAlg::Sha512),
            _ => None,
        }
    }

    /// sha*WithRSAEncryption (some signers name these instead of rsaEncryption).
    fn from_rsa_oid(oid: &[u8]) -> Option<HashAlg> {
        let last = *oid.strip_prefix(OID_PKCS1_PREFIX)?.first()?;
        match (oid.len() == OID_PKCS1_PREFIX.len() + 1, last) {
            (true, 5) => Some(HashAlg::Sha1),
            (true, 11) => Some(HashAlg::Sha256),
            (true, 12) => Some(HashAlg::Sha384),
            (true, 13) => Some(HashAlg::Sha512),
            _ => None,
        }
    }

    fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            HashAlg::Sha1 => Sha1::digest(data).to_vec(),
            HashAlg::Sha256 => Sha256::digest(data).to_vec(),
            HashAlg::Sha384 => Sha384::digest(data).to_vec(),
            HashAlg::Sha512 => Sha512::digest(data).to_vec(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Curve {
    P256,
    P384,
}

enum Key<'a> {
    Rsa { n: &'a [u8], e: &'a [u8] },
    Ec { curve: Curve, point: &'a [u8] },
    Other,
}

struct Certificate<'a> {
    serial: &'a [u8],
    issuer_raw: &'a [u8],
    subject_raw: &'a [u8],
    issuer_name: String,
    subject_name: String,
    /// Subject key identifier extension, if any.
    key_id: Option<&'a [u8]>,
    key: Key<'a>,
}

impl<'a> Certificate<'a> {
    /// Certificate { tbsCertificate { [0] version, serial, sigAlg, issuer, validity,
    /// subject, subjectPublicKeyInfo, ... [3] extensions }, sigAlg, signature }
    fn parse(cert: &Tlv<'a>) -> Option<Certificate<'a>> {
        let tbs = cert.children().next()?;
        let mut f: Vec<Tlv<'a>> = tbs.children().collect();
        if f.first()?.tag == 0xA0 {
            f.remove(0);
        }
        let (serial, issuer, subject, spki) = (f.first()?, f.get(2)?, f.get(4)?, f.get(5)?);
        let mut spki_parts = spki.children();
        let alg = spki_parts.next()?;
        let bits = spki_parts.next()?.value.get(1..)?; // BIT STRING: skip the unused-bits byte
        let mut alg_parts = alg.children();
        let key = match alg_parts.next()?.value {
            OID_RSA => {
                let (seq, _) = Tlv::read(bits)?;
                let mut ints = seq.children();
                let n = ints.next()?.value;
                let e = ints.next()?.value;
                Key::Rsa { n, e }
            }
            OID_EC_PUBLIC_KEY => match alg_parts.next()?.value {
                OID_P256 => Key::Ec { curve: Curve::P256, point: bits },
                OID_P384 => Key::Ec { curve: Curve::P384, point: bits },
                _ => Key::Other,
            },
            _ => Key::Other,
        };
        let key_id = f.iter().find(|t| t.tag == 0xA3).and_then(|ext| {
            ext.children().next()?.children().find_map(|e| {
                let mut p = e.children();
                (p.next()?.value == OID_SUBJECT_KEY_ID).then(|| {
                    let wrapped = p.last()?; // OCTET STRING holding an OCTET STRING
                    Tlv::read(wrapped.value).map(|(t, _)| t.value)
                })?
            })
        });
        Some(Certificate {
            serial: serial.value,
            issuer_raw: issuer.raw,
            subject_raw: subject.raw,
            issuer_name: name(issuer),
            subject_name: name(subject),
            key_id,
            key,
        })
    }

    /// SignerIdentifier: IssuerAndSerialNumber, or [0] SubjectKeyIdentifier.
    fn matches(&self, sid: &Tlv) -> bool {
        match sid.tag {
            SEQUENCE => {
                let mut p = sid.children();
                p.next().is_some_and(|i| i.raw == self.issuer_raw) && p.next().is_some_and(|s| s.value == self.serial)
            }
            0x80 => self.key_id == Some(sid.value),
            _ => false,
        }
    }
}

/// The common name of an X.501 name (else the organisation, else empty).
fn name(n: &Tlv) -> String {
    let attr = |oid: &[u8]| {
        n.children().flat_map(|rdn| rdn.children()).find_map(|atv| {
            let mut p = atv.children();
            (p.next()?.value == oid).then(|| p.next().map(|v| decode_string(v.tag, v.value)))?
        })
    };
    attr(OID_CN).or_else(|| attr(OID_O)).unwrap_or_default()
}

fn decode_string(tag: u8, v: &[u8]) -> String {
    match tag {
        // BMPString: UTF-16BE
        0x1E => String::from_utf16_lossy(
            &v.chunks(2).map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)])).collect::<Vec<_>>(),
        ),
        _ => String::from_utf8_lossy(v).into_owned(),
    }
}

const SEQUENCE: u8 = 0x30;
const SET: u8 = 0x31;
const OID: u8 = 0x06;
const OCTET_STRING: u8 = 0x04;

const OID_SIGNED_DATA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x02];
const OID_MESSAGE_DIGEST: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x04];
const OID_SHA1: &[u8] = &[0x2B, 0x0E, 0x03, 0x02, 0x1A];
const OID_SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
const OID_SHA384: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02];
const OID_SHA512: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03];
const OID_PKCS1_PREFIX: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01];
const OID_RSA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];
const OID_RSA_PSS: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x0A];
const OID_EC_PUBLIC_KEY: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
const OID_ECDSA_PREFIX: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04];
const OID_P256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
const OID_P384: &[u8] = &[0x2B, 0x81, 0x04, 0x00, 0x22];
const OID_CN: &[u8] = &[0x55, 0x04, 0x03];
const OID_O: &[u8] = &[0x55, 0x04, 0x0A];
const OID_SUBJECT_KEY_ID: &[u8] = &[0x55, 0x1D, 0x0E];

/// One DER (or BER, with indefinite lengths) element.
#[derive(Clone, Copy, Debug)]
struct Tlv<'a> {
    tag: u8,
    /// The contents.
    value: &'a [u8],
    /// The whole element, header included.
    raw: &'a [u8],
}

impl<'a> Tlv<'a> {
    /// Reads one element; returns it and the bytes after it.
    fn read(buf: &'a [u8]) -> Option<(Tlv<'a>, &'a [u8])> {
        let tag = *buf.first()?;
        if tag & 0x1F == 0x1F {
            return None; // multi-byte tags do not occur in these structures
        }
        let first = *buf.get(1)?;
        let (len, header) = if first < 0x80 {
            (Some(first as usize), 2)
        } else if first == 0x80 {
            (None, 2)
        } else {
            let n = (first & 0x7F) as usize;
            if n > 4 {
                return None;
            }
            let bytes = buf.get(2..2 + n)?;
            (Some(bytes.iter().fold(0usize, |acc, &b| (acc << 8) | b as usize)), 2 + n)
        };
        match len {
            Some(len) => {
                let end = header.checked_add(len)?;
                let value = buf.get(header..end)?;
                Some((Tlv { tag, value, raw: &buf[..end] }, &buf[end..]))
            }
            None => {
                // Children up to the end-of-contents marker (00 00).
                let mut rest = &buf[header..];
                loop {
                    if rest.starts_with(&[0, 0]) {
                        let used = buf.len() - rest.len();
                        let value = &buf[header..used];
                        return Some((Tlv { tag, value, raw: &buf[..used + 2] }, &rest[2..]));
                    }
                    rest = Tlv::read(rest)?.1;
                }
            }
        }
    }

    fn children(&self) -> Children<'a> {
        Children { rest: if self.tag & 0x20 != 0 { self.value } else { &[] } }
    }
}

struct Children<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Children<'a> {
    type Item = Tlv<'a>;
    fn next(&mut self) -> Option<Tlv<'a>> {
        if self.rest.is_empty() || self.rest.starts_with(&[0, 0]) {
            return None;
        }
        let (t, rest) = Tlv::read(self.rest)?;
        self.rest = rest;
        Some(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn der_lengths_and_children() {
        // SEQUENCE { INTEGER 5, OCTET STRING (130 bytes) } with a long-form length.
        let mut b = vec![0x30, 0x81, 0x88, 0x02, 0x01, 0x05, 0x04, 0x81, 0x82];
        b.extend([7u8; 130]);
        let (seq, rest) = Tlv::read(&b).unwrap();
        assert!(rest.is_empty());
        let kids: Vec<Tlv> = seq.children().collect();
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].value, &[5]);
        assert_eq!(kids[1].value.len(), 130);
        // BER indefinite length, as some signers write.
        let ber = [0x30, 0x80, 0x02, 0x01, 0x05, 0x00, 0x00, 0xFF];
        let (seq, rest) = Tlv::read(&ber).unwrap();
        assert_eq!(rest, &[0xFF]);
        assert_eq!(seq.children().count(), 1);
        assert_eq!(seq.raw.len(), 7);
    }

    /// The fixture's byte ranges and decoded /Contents.
    fn fixture(name: &str) -> (Vec<u8>, Vec<(usize, usize)>, Vec<u8>) {
        let file =
            std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap();
        let start = file.windows(11).position(|w| w == b"/Contents <").unwrap() + 10;
        let end = start + file[start..].iter().position(|&b| b == b'>').unwrap() + 1;
        let hex = std::str::from_utf8(&file[start + 1..end - 1]).unwrap();
        let contents = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        let ranges = vec![(0, start), (end, file.len() - end)];
        (file, ranges, contents)
    }

    #[test]
    fn signatures_verify_and_a_changed_signing_time_breaks_them() {
        for name in ["signed.pdf", "signed-ec.pdf"] {
            let (file, ranges, mut contents) = fixture(name);
            let ok = check(&file, &ranges, &contents, "adbe.pkcs7.detached");
            assert_eq!(ok.status, SigStatus::Valid, "{name}");
            assert_eq!(ok.issuer, "magic-pdf Test Signer");
            // The signing time is a signed attribute: changing it leaves the
            // document's digest intact but breaks the signature itself.
            let time = contents.windows(2).rposition(|w| w == [0x17, 0x0D]).expect("UTCTime") + 2;
            contents[time] = if contents[time] == b'9' { b'8' } else { contents[time] + 1 };
            let bad = check(&file, &ranges, &contents, "adbe.pkcs7.detached");
            assert!(
                matches!(&bad.status, SigStatus::Invalid(m) if m.contains("does not match")),
                "{name}: {:?}",
                bad.status
            );
        }
    }

    #[test]
    fn rsa_algorithm_names() {
        let mut oid = OID_PKCS1_PREFIX.to_vec();
        oid.push(11);
        assert_eq!(HashAlg::from_rsa_oid(&oid), Some(HashAlg::Sha256));
        assert_eq!(HashAlg::from_rsa_oid(OID_RSA), None);
        let garbage = check(b"%PDF", &[(0, 4)], &[1, 2, 3], "adbe.pkcs7.detached");
        assert!(matches!(garbage.status, SigStatus::Unknown(_)));
        let other = check(b"%PDF", &[(0, 4)], &[], "adbe.x509.rsa_sha1");
        assert!(matches!(other.status, SigStatus::Unknown(m) if m.contains("format")));
    }
}
