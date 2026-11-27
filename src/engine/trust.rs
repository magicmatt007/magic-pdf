//! Whether a signer's certificate can be trusted: a chain of valid certificates up to
//! a root certificate the computer trusts, not expired at the time checked and (on
//! Windows) not revoked.
//!
//! Windows builds and checks the chain itself (CryptoAPI, with the Windows root store
//! and revocation lists). Elsewhere magic-pdf builds the chain from the certificates
//! in the signature and the system's root certificates (the PEM bundle that OpenSSL
//! uses); revocation is not checked there.

use super::signatures::{SignatureCheck, Timestamp};

/// The outcome of checking a certificate.
#[derive(Clone, Debug, PartialEq)]
pub enum Trust {
    /// A chain up to a trusted root. `revocation_checked`: the issuers confirmed that
    /// no certificate in it was revoked.
    Trusted { revocation_checked: bool },
    /// The chain ends at a root certificate this computer does not trust (also a
    /// self-made certificate).
    UntrustedRoot,
    /// An issuing certificate is missing (neither in the signature nor known here).
    IssuerUnknown,
    /// A certificate in the chain was not valid at the time checked.
    Expired,
    /// A certificate in the chain was revoked by its issuer (checked on Windows only).
    #[cfg_attr(not(windows), allow(dead_code))]
    Revoked,
    /// The check was not possible.
    Unknown,
}

/// Checks a signature's certificate: at the time of its timestamp when the timestamp
/// is intact and its authority trusted, else now.
pub fn signature_trust(check: &SignatureCheck) -> Trust {
    let at = check.timestamp.as_ref().filter(|ts| timestamp_trusted(ts)).map(|ts| ts.time.as_str());
    evaluate(&check.chain, at)
}

/// The timestamp's authority is trusted (at the time it gave).
pub fn timestamp_trusted(ts: &Timestamp) -> bool {
    ts.valid && matches!(evaluate(&ts.chain, Some(&ts.time)), Trust::Trusted { .. })
}

/// Checks `chain` (the certificate in question first, then others that may issue
/// it), at the PDF date `at` (`D:YYYYMMDDHHMMSS…`) or now.
pub fn evaluate(chain: &[Vec<u8>], at: Option<&str>) -> Trust {
    if chain.is_empty() {
        return Trust::Unknown;
    }
    platform::evaluate(chain, at)
}

/// `D:YYYYMMDDHHMMSS…` as YYYY-MM-DD (the precision of the validity check here).
#[cfg_attr(windows, allow(dead_code))]
fn day(at: Option<&str>) -> String {
    match at.map(|d| d.trim_start_matches("D:")).filter(|d| d.len() >= 8) {
        Some(d) => format!("{}-{}-{}", &d[0..4], &d[4..6], &d[6..8]),
        None => chrono::Utc::now().format("%Y-%m-%d").to_string(),
    }
}

/// Builds and checks the chain against `roots` (DER).
#[cfg_attr(windows, allow(dead_code))]
pub(super) fn evaluate_with(chain: &[Vec<u8>], roots: &[Vec<u8>], at: Option<&str>) -> Trust {
    use super::signatures::{Certificate, Tlv, issued_by};
    fn parse(der: &[u8]) -> Option<Tlv<'_>> {
        Tlv::read(der).map(|(t, _)| t)
    }
    let tlvs: Vec<_> = chain.iter().filter_map(|d| parse(d)).collect();
    let root_tlvs: Vec<_> = roots.iter().filter_map(|d| parse(d)).collect();
    let certs: Vec<Certificate> = tlvs.iter().filter_map(Certificate::parse).collect();
    let root_certs: Vec<Certificate> = root_tlvs.iter().filter_map(Certificate::parse).collect();
    let Some(mut current) = certs.first() else { return Trust::Unknown };
    let today = day(at);
    let valid = |c: &Certificate| {
        (c.not_before.is_empty() || c.not_before.as_str() <= today.as_str())
            && (c.not_after.is_empty() || today.as_str() <= c.not_after.as_str())
    };
    for _ in 0..10 {
        if !valid(current) {
            return Trust::Expired;
        }
        // The certificate itself is a trusted root.
        if root_certs.iter().any(|r| r.subject_raw == current.subject_raw && r.tbs == current.tbs) {
            return Trust::Trusted { revocation_checked: false };
        }
        if root_certs.iter().any(|r| issued_by(current, r)) {
            return Trust::Trusted { revocation_checked: false };
        }
        match certs.iter().find(|c| c.subject_raw != current.subject_raw && issued_by(current, c)) {
            Some(issuer) => current = issuer,
            None if current.subject_raw == current.issuer_raw => return Trust::UntrustedRoot,
            None => return Trust::IssuerUnknown,
        }
    }
    Trust::Unknown
}

#[cfg(not(windows))]
mod platform {
    use std::sync::OnceLock;

    pub fn evaluate(chain: &[Vec<u8>], at: Option<&str>) -> super::Trust {
        let roots = system_roots();
        if roots.is_empty() {
            return super::Trust::Unknown;
        }
        super::evaluate_with(chain, roots, at)
    }

    /// The system's root certificates (DER), read once.
    fn system_roots() -> &'static [Vec<u8>] {
        static ROOTS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
        ROOTS.get_or_init(|| {
            // Debian/Ubuntu, Fedora/RHEL, openSUSE, Alpine, macOS.
            const BUNDLES: &[&str] = &[
                "/etc/ssl/certs/ca-certificates.crt",
                "/etc/pki/tls/certs/ca-bundle.crt",
                "/etc/ssl/ca-bundle.pem",
                "/etc/ssl/cert.pem",
            ];
            let from_env = std::env::var("SSL_CERT_FILE").ok();
            from_env
                .iter()
                .map(String::as_str)
                .chain(BUNDLES.iter().copied())
                .find_map(|p| std::fs::read_to_string(p).ok())
                .map(|pem| pem_certificates(&pem))
                .unwrap_or_default()
        })
    }

    /// The certificates in a PEM bundle.
    pub(super) fn pem_certificates(pem: &str) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut rest = pem;
        while let Some(start) = rest.find("-----BEGIN CERTIFICATE-----") {
            let body = &rest[start + 27..];
            let Some(end) = body.find("-----END CERTIFICATE-----") else { break };
            if let Some(der) = base64(&body[..end]) {
                out.push(der);
            }
            rest = &body[end..];
        }
        out
    }

    fn base64(text: &str) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let (mut acc, mut bits) = (0u32, 0);
        for c in text.bytes().filter(|c| !c.is_ascii_whitespace()) {
            let v = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => break,
                _ => return None,
            };
            acc = (acc << 6) | v as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        Some(out)
    }
}

#[cfg(windows)]
mod platform {
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Security::Cryptography::*;

    use super::Trust;

    // CERT_TRUST_STATUS.dwErrorStatus flags (wincrypt.h).
    const NOT_TIME_VALID: u32 = 0x1;
    const REVOKED: u32 = 0x4;
    const NOT_SIGNATURE_VALID: u32 = 0x8;
    const UNTRUSTED_ROOT: u32 = 0x20;
    const REVOCATION_UNKNOWN: u32 = 0x40;
    const PARTIAL_CHAIN: u32 = 0x10000;
    const OFFLINE_REVOCATION: u32 = 0x1000000;

    pub fn evaluate(chain: &[Vec<u8>], at: Option<&str>) -> Trust {
        unsafe {
            let encoding = X509_ASN_ENCODING | PKCS_7_ASN_ENCODING;
            let leaf = CertCreateCertificateContext(encoding, chain[0].as_ptr(), chain[0].len() as u32);
            if leaf.is_null() {
                return Trust::Unknown;
            }
            // The other certificates of the signature, for building the chain.
            let extra = CertOpenStore(CERT_STORE_PROV_MEMORY, 0, 0, 0, null());
            for der in &chain[1..] {
                CertAddEncodedCertificateToStore(
                    extra,
                    encoding,
                    der.as_ptr(),
                    der.len() as u32,
                    CERT_STORE_ADD_ALWAYS,
                    null_mut(),
                );
            }
            let mut para: CERT_CHAIN_PARA = std::mem::zeroed();
            para.cbSize = std::mem::size_of::<CERT_CHAIN_PARA>() as u32;
            // Fetching revocation lists may take a moment; not forever.
            para.dwUrlRetrievalTimeout = 15_000;
            let time = at.and_then(filetime);
            let mut context: *mut CERT_CHAIN_CONTEXT = null_mut();
            let ok = CertGetCertificateChain(
                null_mut(),
                leaf,
                time.as_ref().map_or(null(), |t| t as *const FILETIME),
                extra,
                &para,
                CERT_CHAIN_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT,
                null(),
                &mut context,
            );
            let result = if ok == 0 || context.is_null() {
                Trust::Unknown
            } else {
                let status = (*context).TrustStatus.dwErrorStatus;
                CertFreeCertificateChain(context);
                if status & REVOKED != 0 {
                    Trust::Revoked
                } else if status & NOT_TIME_VALID != 0 {
                    Trust::Expired
                } else if status & PARTIAL_CHAIN != 0 {
                    Trust::IssuerUnknown
                } else if status & UNTRUSTED_ROOT != 0 {
                    Trust::UntrustedRoot
                } else if status & NOT_SIGNATURE_VALID != 0 {
                    Trust::Unknown
                } else {
                    Trust::Trusted { revocation_checked: status & (REVOCATION_UNKNOWN | OFFLINE_REVOCATION) == 0 }
                }
            };
            CertCloseStore(extra, 0);
            CertFreeCertificateContext(leaf);
            result
        }
    }

    /// `D:YYYYMMDDHHMMSS…` (UTC) as a FILETIME.
    fn filetime(pdf_date: &str) -> Option<FILETIME> {
        let d = pdf_date.trim_start_matches("D:");
        let digits: String = d.chars().take_while(char::is_ascii_digit).collect();
        let t = chrono::NaiveDateTime::parse_from_str(digits.get(..14)?, "%Y%m%d%H%M%S").ok()?.and_utc();
        // 100-ns intervals since 1601-01-01.
        let ticks = (t.timestamp() + 11_644_473_600) as u64 * 10_000_000;
        Some(FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
    }

    #[test]
    fn chains_are_built_and_checked() {
        let (root, inter, leaf) = (fixture("chain-root.der"), fixture("chain-inter.der"), fixture("chain-leaf.der"));
        let roots = vec![root.clone()];
        let at = Some("D:20270101120000Z");
        // Leaf, then its issuer: up to the trusted root.
        assert_eq!(
            evaluate_with(&[leaf.clone(), inter.clone()], &roots, at),
            Trust::Trusted { revocation_checked: false }
        );
        // Without the intermediate the chain is incomplete.
        assert_eq!(evaluate_with(std::slice::from_ref(&leaf), &roots, at), Trust::IssuerUnknown);
        // A root nobody here trusts.
        assert_eq!(evaluate_with(&[leaf.clone(), inter.clone(), root.clone()], &[], at), Trust::UntrustedRoot);
        // Outside the leaf's validity (2026-2036).
        assert_eq!(evaluate_with(&[leaf.clone(), inter.clone()], &roots, Some("D:20400101000000Z")), Trust::Expired);
        // A self-made certificate.
        assert_eq!(evaluate_with(&[fixture("cert-email.der")], &roots, at), Trust::UntrustedRoot);
    }

    #[cfg(not(windows))]
    #[test]
    fn pem_bundles_are_read() {
        let der = fixture("chain-root.der");
        let b64 = |d: &[u8]| {
            const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut s = String::new();
            for c in d.chunks(3) {
                let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
                for i in 0..4 {
                    s.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
                }
            }
            s
        };
        let pem = format!("junk\n-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", b64(&der));
        assert_eq!(platform::pem_certificates(&pem), vec![der]);
    }
}
