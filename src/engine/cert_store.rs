//! Signing with certificates from the Windows certificate store (the user's
//! "Personal" store, as Acrobat uses it), including keys on smart cards: Windows
//! does the signing and asks for a PIN itself when the key needs one.

use crate::i18n::{t, tf};
use std::ptr::{null, null_mut};

use sha1::{Digest, Sha1};
use windows_sys::Win32::Security::Cryptography::*;

use super::sign::{ExternalKey, KeyKind, SigningIdentity, certificate_key_kind};

/// A certificate in the store that has a private key magic-pdf can sign with.
#[derive(Clone, Debug)]
pub struct StoreCertificate {
    /// SHA-1 of the certificate, as Windows shows it ("thumbprint").
    pub thumbprint: String,
    pub name: String,
    pub issuer: String,
    pub expires: String,
}

/// An open "MY" store of the current user (closed on drop).
struct Store(HCERTSTORE);

impl Store {
    fn open() -> Result<Store, String> {
        let name: Vec<u16> = "MY\0".encode_utf16().collect();
        let h = unsafe {
            CertOpenStore(
                CERT_STORE_PROV_SYSTEM_W,
                0,
                0,
                CERT_SYSTEM_STORE_CURRENT_USER | CERT_STORE_READONLY_FLAG,
                name.as_ptr().cast(),
            )
        };
        if h.is_null() { Err(t("the Windows certificate store could not be opened").into()) } else { Ok(Store(h)) }
    }

    /// Calls `f` with each certificate (its context and DER bytes).
    fn each(&self, mut f: impl FnMut(*const CERT_CONTEXT, &[u8]) -> bool) {
        let mut ctx: *const CERT_CONTEXT = null();
        loop {
            ctx = unsafe { CertEnumCertificatesInStore(self.0, ctx) };
            if ctx.is_null() {
                return;
            }
            let der = unsafe { std::slice::from_raw_parts((*ctx).pbCertEncoded, (*ctx).cbCertEncoded as usize) };
            if !f(ctx, der) {
                unsafe { CertFreeCertificateContext(ctx) };
                return;
            }
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        unsafe { CertCloseStore(self.0, 0) };
    }
}

fn has_private_key(ctx: *const CERT_CONTEXT) -> bool {
    let mut size = 0u32;
    unsafe { CertGetCertificateContextProperty(ctx, CERT_KEY_PROV_INFO_PROP_ID, null_mut(), &mut size) != 0 }
}

fn thumbprint(der: &[u8]) -> String {
    Sha1::digest(der).iter().map(|b| format!("{b:02X}")).collect()
}

/// The certificates with a private key, newest expiry first.
pub fn list() -> Result<Vec<StoreCertificate>, String> {
    let store = Store::open()?;
    let mut out = Vec::new();
    store.each(|ctx, der| {
        if has_private_key(ctx)
            && certificate_key_kind(der).is_some()
            && let Some((name, issuer, expires)) = super::sign::certificate_summary(der)
        {
            out.push(StoreCertificate { thumbprint: thumbprint(der), name, issuer, expires });
        }
        true
    });
    out.sort_by(|a, b| b.expires.cmp(&a.expires));
    Ok(out)
}

/// The certificate with this thumbprint, ready to sign.
pub fn identity(thumb: &str) -> Result<SigningIdentity, String> {
    let store = Store::open()?;
    let mut found: Option<*const CERT_CONTEXT> = None;
    store.each(|ctx, der| {
        if thumbprint(der).eq_ignore_ascii_case(thumb) {
            found = Some(unsafe { CertDuplicateCertificateContext(ctx) });
            return false;
        }
        true
    });
    let ctx = found.ok_or(t("the certificate is no longer in the Windows certificate store"))?;
    let key = StoreKey(ctx);
    let der = key.der().to_vec();
    let kind = certificate_key_kind(&der).ok_or(t("this kind of key is not supported"))?;
    let chain = chain(ctx).unwrap_or_else(|| vec![der]);
    SigningIdentity::from_external(chain, Box::new(StoreKeyWithKind { key, kind }))
        .ok_or_else(|| t("the certificate cannot be read").to_string())
}

/// The certificate chain as Windows builds it (signer first).
fn chain(ctx: *const CERT_CONTEXT) -> Option<Vec<Vec<u8>>> {
    let mut para: CERT_CHAIN_PARA = unsafe { std::mem::zeroed() };
    para.cbSize = std::mem::size_of::<CERT_CHAIN_PARA>() as u32;
    let mut chain: *mut CERT_CHAIN_CONTEXT = null_mut();
    let ok = unsafe { CertGetCertificateChain(null_mut(), ctx, null(), null_mut(), &para, 0, null(), &mut chain) };
    if ok == 0 || chain.is_null() {
        return None;
    }
    let mut out = Vec::new();
    unsafe {
        if (*chain).cChain > 0 {
            let simple = *(*chain).rgpChain;
            let n = (*simple).cElement as usize;
            for i in 0..n {
                let el = *(*simple).rgpElement.add(i);
                let c = (*el).pCertContext;
                out.push(std::slice::from_raw_parts((*c).pbCertEncoded, (*c).cbCertEncoded as usize).to_vec());
            }
        }
        CertFreeCertificateChain(chain);
    }
    (!out.is_empty()).then_some(out)
}

/// A certificate context from the store (freed on drop).
struct StoreKey(*const CERT_CONTEXT);

// The context is only read, and CryptoAPI contexts may be used from any thread.
unsafe impl Send for StoreKey {}
unsafe impl Sync for StoreKey {}

impl StoreKey {
    fn der(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts((*self.0).pbCertEncoded, (*self.0).cbCertEncoded as usize) }
    }
}

impl Drop for StoreKey {
    fn drop(&mut self) {
        unsafe { CertFreeCertificateContext(self.0) };
    }
}

struct StoreKeyWithKind {
    key: StoreKey,
    kind: KeyKind,
}

impl ExternalKey for StoreKeyWithKind {
    fn kind(&self) -> KeyKind {
        self.kind
    }

    fn sign_digest(&self, digest: &[u8]) -> Result<Vec<u8>, String> {
        let mut handle: HCRYPTPROV_OR_NCRYPT_KEY_HANDLE = 0;
        let mut spec: CERT_KEY_SPEC = 0;
        let mut free: windows_sys::core::BOOL = 0;
        let ok = unsafe {
            CryptAcquireCertificatePrivateKey(
                self.key.0,
                CRYPT_ACQUIRE_ONLY_NCRYPT_KEY_FLAG,
                null(),
                &mut handle,
                &mut spec,
                &mut free,
            )
        };
        if ok == 0 || spec != CERT_NCRYPT_KEY_SPEC {
            return Err(t("Windows did not give access to the certificate's private key").into());
        }
        let padding = BCRYPT_PKCS1_PADDING_INFO {
            pszAlgId: if self.kind == KeyKind::P384 { BCRYPT_SHA384_ALGORITHM } else { BCRYPT_SHA256_ALGORITHM },
        };
        let (info, flags) = match self.kind {
            KeyKind::Rsa => ((&padding as *const BCRYPT_PKCS1_PADDING_INFO).cast(), BCRYPT_PAD_PKCS1),
            _ => (null(), 0),
        };
        let mut size = 0u32;
        let sign = |buf: *mut u8, len: u32, size: &mut u32| unsafe {
            NCryptSignHash(handle, info, digest.as_ptr(), digest.len() as u32, buf, len, size, flags)
        };
        let mut result = sign(null_mut(), 0, &mut size);
        let mut sig = vec![0u8; size as usize];
        if result == 0 {
            result = sign(sig.as_mut_ptr(), size, &mut size);
            sig.truncate(size as usize);
        }
        if free != 0 {
            unsafe { NCryptFreeObject(handle) };
        }
        if result != 0 {
            return Err(tf("Windows could not sign (error {code})", &[("code", &format!("0x{:08X}", result as u32))]));
        }
        Ok(sig)
    }
}
