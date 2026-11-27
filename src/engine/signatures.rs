//! Checking digital signatures of PDFs (detached CMS / PKCS#7 signatures, the kind
//! Acrobat and most signing tools write): that the signed bytes are unchanged, that
//! the signature matches the signer's certificate, and whether the file was changed
//! after signing. Whether the certificate itself is trusted (a chain up to a
//! certificate authority, revocation) is not checked.
//!
//! The CMS structures are read with a small DER reader below; only the RSA and ECDSA
//! maths come from the RustCrypto crates.

use crate::i18n::t;
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
    /// The certificates in the signature (DER), the signer's first: for checking
    /// whether it can be trusted.
    pub chain: Vec<Vec<u8>>,
    /// A trusted timestamp (RFC 3161) on the signature, if it has one.
    pub timestamp: Option<Timestamp>,
}

/// A timestamp token: when a timestamp authority saw the signature.
#[derive(Clone, Debug, PartialEq)]
pub struct Timestamp {
    /// As a PDF date (UTC), e.g. `D:20261127100000Z`.
    pub time: String,
    /// The timestamp authority (its certificate's name).
    pub authority: String,
    /// The token is over this signature and its own signature is intact.
    pub valid: bool,
    /// The authority's certificates (DER), its own first.
    pub chain: Vec<Vec<u8>>,
}

/// Checks a signature. `ranges` are the (offset, length) pairs of the signature's
/// /ByteRange in `file`; `contents` is the decoded /Contents.
pub fn check(file: &[u8], ranges: &[(usize, usize)], contents: &[u8], subfilter: &str) -> SignatureCheck {
    let mut out = SignatureCheck {
        status: SigStatus::Unknown(""),
        signer: String::new(),
        issuer: String::new(),
        self_signed: false,
        chain: Vec::new(),
        timestamp: None,
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
        return Err(unknown(t("this signature format is not supported")));
    }
    // The signed bytes.
    let mut data = Vec::new();
    for &(start, len) in ranges {
        let part = start
            .checked_add(len)
            .and_then(|end| file.get(start..end))
            .ok_or(Problem::Invalid(t("the signed range lies outside the file")))?;
        data.extend_from_slice(part);
    }
    let end = ranges.iter().map(|&(s, l)| s + l).max().unwrap_or(0);
    let covers_whole =
        ranges.first().is_some_and(|r| r.0 == 0) && file[end.min(file.len())..].iter().all(u8::is_ascii_whitespace);

    // ContentInfo { contentType signedData, [0] SignedData }
    let (content_info, _) = Tlv::read(contents).ok_or(unknown(t("the signature data cannot be read")))?;
    let mut ci = content_info.children();
    let oid = ci.next().filter(|t| t.tag == OID).ok_or(unknown(t("not a CMS signature")))?;
    if oid.value != OID_SIGNED_DATA {
        return Err(unknown(t("not a CMS signature")));
    }
    let explicit = ci.next().filter(|t| t.tag == 0xA0).ok_or(unknown(t("CMS signature without content")))?;
    let signed_data = explicit.children().next().ok_or(unknown(t("CMS signature without content")))?;
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
    let signer_info =
        signer_infos.and_then(|s| s.children().next()).ok_or(unknown(t("the signature has no signer")))?;
    // SignerInfo { version, sid, digestAlgorithm, [0] signedAttrs, signatureAlgorithm, signature, ... }
    let parts: Vec<Tlv> = signer_info.children().collect();
    let sid = parts.get(1).ok_or(unknown(t("the signer is not named")))?;
    let digest_oid = parts.get(2).and_then(|a| a.children().next()).ok_or(unknown(t("no digest algorithm")))?;
    let hash = HashAlg::from_oid(digest_oid.value).ok_or(unknown(t("the digest algorithm is not supported")))?;
    let signed_attrs = parts.iter().find(|p| p.tag == 0xA0);
    let after_attrs = parts.iter().skip(3).filter(|p| p.tag != 0xA0);
    let mut rest = after_attrs;
    let sig_alg = rest.next().and_then(|a| a.children().next()).ok_or(unknown(t("no signature algorithm")))?;
    let signature = rest.next().filter(|s| s.tag == OCTET_STRING).ok_or(unknown(t("no signature value")))?;
    let unsigned_attrs = rest.next().filter(|a| a.tag == 0xA1);

    // The signer's certificate: matched by issuer and serial number, or by key identifier.
    let cert = certs
        .iter()
        .filter_map(Certificate::parse)
        .find(|c| c.matches(sid))
        .ok_or(unknown(t("the signer's certificate is not included")))?;
    out.signer = cert.subject_name.clone();
    out.issuer = cert.issuer_name.clone();
    out.self_signed = cert.subject_raw == cert.issuer_raw;
    let signer_raw =
        certs.iter().find(|c| Certificate::parse(c).is_some_and(|p| p.matches(sid))).map(|c| c.raw.to_vec());
    out.chain = signer_raw
        .iter()
        .cloned()
        .chain(certs.iter().map(|c| c.raw.to_vec()).filter(|c| Some(c) != signer_raw.as_ref()))
        .collect();
    out.timestamp = unsigned_attrs
        .and_then(|attrs| {
            attrs.children().find_map(|attr| {
                let mut a = attr.children();
                (a.next()?.value == OID_TIMESTAMP_TOKEN).then(|| a.next()?.children().next())?
            })
        })
        .and_then(|token| timestamp(&token, signature.value));

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
                .ok_or(unknown(t("the signature does not record the document's digest")))?;
            if message_digest != digest.as_slice() {
                return Err(Problem::Invalid(t("the document was changed after it was signed")));
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
        (_, oid) if oid == OID_RSA_PSS => return Err(unknown(t("RSA-PSS signatures are not supported"))),
        _ => return Err(unknown(t("the signature algorithm is not supported"))),
    };
    if !ok {
        return Err(Problem::Invalid(t("the signature does not match the signer's certificate")));
    }
    Ok(if covers_whole { SigStatus::Valid } else { SigStatus::ChangedAfter })
}

fn verify_rsa(n: &[u8], e: &[u8], hash: HashAlg, hashed: &[u8], sig: &[u8]) -> Result<bool, Problem> {
    use rsa::{BoxedUint, Pkcs1v15Sign, PublicKey};
    let key = PublicKey::new(BoxedUint::from_be_slice_vartime(n), BoxedUint::from_be_slice_vartime(e))
        .map_err(|_| unknown(t("the RSA key is not usable")))?;
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
    let (seq, _) = Tlv::read(sig).ok_or(Problem::Invalid(t("the signature value cannot be read")))?;
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
            let key = VerifyingKey::from_sec1_bytes(point).map_err(|_| unknown(t("the EC key is not usable")))?;
            Signature::from_slice(&rs).is_ok_and(|s| key.verify_prehash(hashed, &s).is_ok())
        }
        Curve::P384 => {
            use p384::ecdsa::{Signature, VerifyingKey, signature::hazmat::PrehashVerifier};
            let key = VerifyingKey::from_sec1_bytes(point).map_err(|_| unknown(t("the EC key is not usable")))?;
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
pub(super) enum Curve {
    P256,
    P384,
}

pub(super) enum Key<'a> {
    Rsa { n: &'a [u8], e: &'a [u8] },
    Ec { curve: Curve, point: &'a [u8] },
    Other,
}

pub(super) struct Certificate<'a> {
    pub(super) serial: &'a [u8],
    pub(super) issuer_raw: &'a [u8],
    pub(super) subject_raw: &'a [u8],
    pub(super) issuer_name: String,
    pub(super) subject_name: String,
    /// Validity as YYYY-MM-DD (empty if unreadable).
    pub(super) not_after: String,
    pub(super) not_before: String,
    /// The certificate may sign documents: its key usage (if stated) allows
    /// signatures, and its purposes (if stated) include e-mail protection, document
    /// signing or any purpose; not e.g. a login-only device certificate.
    pub(super) for_signing: bool,
    /// The signed part (tbsCertificate, DER), the issuer's signature algorithm and
    /// signature over it.
    pub(super) tbs: &'a [u8],
    sig_alg: &'a [u8],
    sig_value: &'a [u8],
    /// Subject key identifier extension, if any.
    key_id: Option<&'a [u8]>,
    pub(super) key: Key<'a>,
}

impl<'a> Certificate<'a> {
    /// Certificate { tbsCertificate { [0] version, serial, sigAlg, issuer, validity,
    /// subject, subjectPublicKeyInfo, ... [3] extensions }, sigAlg, signature }
    pub(super) fn parse(cert: &Tlv<'a>) -> Option<Certificate<'a>> {
        let mut outer = cert.children();
        let tbs = outer.next()?;
        let sig_alg = outer.next()?.children().next()?.value;
        let sig_value = outer.next()?.value.get(1..)?; // BIT STRING: skip the unused-bits byte
        let mut f: Vec<Tlv<'a>> = tbs.children().collect();
        if f.first()?.tag == 0xA0 {
            f.remove(0);
        }
        let (serial, issuer, validity, subject, spki) = (f.first()?, f.get(2)?, f.get(3)?, f.get(4)?, f.get(5)?);
        let not_after = validity.children().nth(1).map(|t| asn1_date(t.tag, t.value)).unwrap_or_default();
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
        // An extension's value: an OCTET STRING holding the DER of the value.
        let extension = |oid: &[u8]| -> Option<Tlv<'a>> {
            f.iter().find(|t| t.tag == 0xA3).and_then(|ext| {
                ext.children().next()?.children().find_map(|e| {
                    let mut p = e.children();
                    (p.next()?.value == oid).then(|| Tlv::read(p.last()?.value).map(|(t, _)| t))?
                })
            })
        };
        let key_id = extension(OID_SUBJECT_KEY_ID).map(|t| t.value);
        // KeyUsage BIT STRING: digitalSignature (bit 0) or nonRepudiation (bit 1).
        let signs = extension(OID_KEY_USAGE).map(|bits| bits.value.get(1).is_some_and(|b| b & 0xC0 != 0));
        let purposes: Option<Vec<&'a [u8]>> =
            extension(OID_EXT_KEY_USAGE).map(|seq| seq.children().map(|o| o.value).collect());
        let not_before = validity.children().next().map(|t| asn1_date(t.tag, t.value)).unwrap_or_default();
        Some(Certificate {
            serial: serial.value,
            issuer_raw: issuer.raw,
            subject_raw: subject.raw,
            issuer_name: name(issuer),
            subject_name: name(subject),
            not_after,
            not_before,
            tbs: tbs.raw,
            sig_alg,
            sig_value,
            for_signing: signs != Some(false)
                && purposes.is_none_or(|p| p.iter().any(|o| SIGNING_PURPOSES.contains(o))),
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

/// Reads a timestamp token (a CMS signature over a TSTInfo) and checks that it is
/// over `signature_value` and intact.
fn timestamp(token: &Tlv, signature_value: &[u8]) -> Option<Timestamp> {
    // ContentInfo { signedData, [0] SignedData { version, digestAlgorithms,
    // encapContentInfo { id-ct-TSTInfo, [0] OCTET STRING (TSTInfo) }, ... } }
    let mut ci = token.children();
    ci.next()?;
    let signed_data = ci.next()?.children().next()?;
    let encap = signed_data.children().nth(2)?;
    let tst_der = encap.children().nth(1)?.children().next()?.value;
    // TSTInfo { version, policy, messageImprint { algorithm, hash }, serial, genTime, ... }
    let (tst, _) = Tlv::read(tst_der)?;
    let f: Vec<Tlv> = tst.children().collect();
    let mut imprint = f.get(2)?.children();
    let hash = HashAlg::from_oid(imprint.next()?.children().next()?.value)?;
    let over_this = imprint.next()?.value == hash.digest(signature_value).as_slice();
    let gen_time = f.get(4).filter(|t| t.tag == 0x18)?;
    let digits: String = String::from_utf8_lossy(gen_time.value).chars().take_while(char::is_ascii_digit).collect();
    // The token's own signature, over the TSTInfo.
    let own = check(tst_der, &[(0, tst_der.len())], token.raw, "adbe.pkcs7.detached");
    Some(Timestamp {
        time: format!("D:{digits}Z"),
        authority: own.signer,
        valid: over_this && own.status == SigStatus::Valid,
        chain: own.chain,
    })
}

/// Whether `cert` names `issuer` as its issuer and carries its valid signature.
pub(super) fn issued_by(cert: &Certificate, issuer: &Certificate) -> bool {
    if cert.issuer_raw != issuer.subject_raw {
        return false;
    }
    let ok = match (&issuer.key, cert.sig_alg) {
        (Key::Rsa { n, e }, oid) => {
            HashAlg::from_rsa_oid(oid).map(|hash| verify_rsa(n, e, hash, &hash.digest(cert.tbs), cert.sig_value))
        }
        (Key::Ec { curve, point }, OID_ECDSA_SHA256) => {
            Some(verify_ecdsa(*curve, point, &HashAlg::Sha256.digest(cert.tbs), cert.sig_value))
        }
        (Key::Ec { curve, point }, OID_ECDSA_SHA384) => {
            Some(verify_ecdsa(*curve, point, &HashAlg::Sha384.digest(cert.tbs), cert.sig_value))
        }
        _ => None,
    };
    matches!(ok, Some(Ok(true)))
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

/// A UTCTime (YYMMDD…, tag 0x17) or GeneralizedTime (YYYYMMDD…) as YYYY-MM-DD.
fn asn1_date(tag: u8, v: &[u8]) -> String {
    let s = String::from_utf8_lossy(v);
    let (year, rest) = match tag {
        0x17 if s.len() >= 6 => {
            let yy: u32 = s[..2].parse().unwrap_or(0);
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, &s[2..])
        }
        0x18 if s.len() >= 8 => (s[..4].parse().unwrap_or(0), &s[4..]),
        _ => return String::new(),
    };
    format!("{year:04}-{}-{}", &rest[..2], &rest[2..4])
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

pub(super) const SEQUENCE: u8 = 0x30;
pub(super) const SET: u8 = 0x31;
const OID: u8 = 0x06;
pub(super) const OCTET_STRING: u8 = 0x04;

pub(super) const OID_SIGNED_DATA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x02];
pub(super) const OID_MESSAGE_DIGEST: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x04];
pub(super) const OID_SHA1: &[u8] = &[0x2B, 0x0E, 0x03, 0x02, 0x1A];
pub(super) const OID_SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
pub(super) const OID_SHA384: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02];
pub(super) const OID_SHA512: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03];
pub(super) const OID_PKCS1_PREFIX: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01];
pub(super) const OID_RSA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];
pub(super) const OID_RSA_PSS: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x0A];
pub(super) const OID_EC_PUBLIC_KEY: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
pub(super) const OID_ECDSA_PREFIX: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04];
pub(super) const OID_P256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
pub(super) const OID_P384: &[u8] = &[0x2B, 0x81, 0x04, 0x00, 0x22];
pub(super) const OID_CN: &[u8] = &[0x55, 0x04, 0x03];
pub(super) const OID_O: &[u8] = &[0x55, 0x04, 0x0A];
pub(super) const OID_SUBJECT_KEY_ID: &[u8] = &[0x55, 0x1D, 0x0E];
const OID_ECDSA_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
const OID_ECDSA_SHA384: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x03];
const OID_TIMESTAMP_TOKEN: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x10, 0x02, 0x0E];
const OID_KEY_USAGE: &[u8] = &[0x55, 0x1D, 0x0F];
const OID_EXT_KEY_USAGE: &[u8] = &[0x55, 0x1D, 0x25];
/// Extended key usages for signing documents: any purpose, e-mail protection,
/// Microsoft document signing, Adobe Authentic Documents, PDF signing (RFC 9336).
const SIGNING_PURPOSES: &[&[u8]] = &[
    &[0x55, 0x1D, 0x25, 0x00],
    &[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x04],
    &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x0A, 0x03, 0x0C],
    &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x2F, 0x01, 0x01, 0x05],
    &[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x24],
];

/// One DER (or BER, with indefinite lengths) element.
#[derive(Clone, Copy, Debug)]
pub(super) struct Tlv<'a> {
    pub(super) tag: u8,
    /// The contents.
    pub(super) value: &'a [u8],
    /// The whole element, header included.
    pub(super) raw: &'a [u8],
}

impl<'a> Tlv<'a> {
    /// Reads one element; returns it and the bytes after it.
    pub(super) fn read(buf: &'a [u8]) -> Option<(Tlv<'a>, &'a [u8])> {
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

    pub(super) fn children(&self) -> Children<'a> {
        Children { rest: if self.tag & 0x20 != 0 { self.value } else { &[] } }
    }
}

pub(super) struct Children<'a> {
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

    /// A timestamp token only counts for the signature it was made for.
    #[test]
    fn a_timestamp_belongs_to_its_signature() {
        let file = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/signed-timestamped.pdf"),
        )
        .unwrap();
        let doc = lopdf::Document::load_mem(&file).unwrap();
        let contents = doc
            .objects
            .values()
            .filter_map(|o| o.as_dict().ok())
            .find_map(|d| d.get(b"Contents").ok()?.as_str().ok().filter(|c| c.len() > 1000).map(<[u8]>::to_vec))
            .unwrap();
        // ContentInfo > [0] SignedData > signerInfos > SignerInfo
        let (ci, _) = Tlv::read(&contents).unwrap();
        let signed_data = ci.children().nth(1).unwrap().children().next().unwrap();
        let signer_info = signed_data.children().filter(|p| p.tag == SET).last().unwrap().children().next().unwrap();
        let parts: Vec<Tlv> = signer_info.children().collect();
        let signature = parts.iter().find(|p| p.tag == OCTET_STRING).unwrap();
        let unsigned = parts.iter().find(|p| p.tag == 0xA1).unwrap();
        let token = unsigned.children().next().unwrap().children().nth(1).unwrap().children().next().unwrap();
        assert!(timestamp(&token, signature.value).unwrap().valid);
        assert!(!timestamp(&token, b"another signature").unwrap().valid);
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
