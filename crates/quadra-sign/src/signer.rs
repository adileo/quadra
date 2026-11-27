//! The signing key with its certificate chain, and certificate helpers shared by the
//! signature formats.

use crate::{oids, p12, pem, SigningTime};
use der::{Decode, Encode, Tagged};
use p256::ecdsa::signature::{Signer as _, Verifier as _};
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey};
use rsa::traits::PublicKeyParts;
use rsa::Pkcs1v15Sign;
use sha2::{Digest, Sha256};
use x509_cert::name::Name;
use x509_cert::Certificate;

const P256_CURVE: der::oid::ObjectIdentifier =
    der::oid::ObjectIdentifier::new_unwrap("1.2.840.10045.3.1.7");

pub(crate) enum Key {
    Rsa(Box<rsa::RsaPrivateKey>),
    P256(p256::ecdsa::SigningKey),
}

/// A private key with its X.509 certificate chain: what a company signs with.
///
/// Build it with [`Signer::from_pkcs12`] (the usual form in which a signing
/// certificate is delivered) or [`Signer::from_pem`]. The private key is checked
/// against the certificate when the `Signer` is built. The key itself is never shown,
/// not even by `Debug`.
pub struct Signer {
    key: Key,
    /// Signer certificate first, then its issuers as far as they were given.
    chain: Vec<Certificate>,
    chain_der: Vec<Vec<u8>>,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("subject", &self.subject_dn())
            .field("issuer", &self.issuer_dn())
            .field("not_after", &self.not_after())
            .field(
                "key",
                &match self.key {
                    Key::Rsa(_) => "RSA",
                    Key::P256(_) => "ECDSA P-256",
                },
            )
            .finish()
    }
}

impl Signer {
    /// Reads a PKCS#12 file (`.p12` / `.pfx`) holding the private key and its
    /// certificates.
    ///
    /// Both the current format (AES-256-CBC with PBKDF2, HMAC-SHA256 or PBMAC1
    /// integrity, the default of OpenSSL 3 and later) and the legacy one (3DES or RC2-40
    /// with a SHA-1 MAC) are read. A wrong password gives a clear error, as does a file
    /// whose key matches none of its certificates.
    pub fn from_pkcs12(p12: &[u8], password: &str) -> Result<Signer, String> {
        let contents = p12::parse(p12, password)?;
        let key = key_from_pkcs8(&contents.key_pkcs8)?;
        Signer::assemble(key, contents.certificates, "the PKCS#12 file")
    }

    /// Reads a PEM private key and a PEM certificate chain.
    ///
    /// The key may be PKCS#8 (`BEGIN PRIVATE KEY`), PKCS#1 RSA (`BEGIN RSA PRIVATE
    /// KEY`) or SEC1 EC P-256 (`BEGIN EC PRIVATE KEY`); encrypted keys are refused. The
    /// chain holds one or more `BEGIN CERTIFICATE` blocks, the signer's certificate first
    /// and then its issuers (the order is checked against the key and the issuer names,
    /// so a chain in another order still works).
    pub fn from_pem(key_pem: &str, chain_pem: &str) -> Result<Signer, String> {
        let blocks = pem::blocks(key_pem, "the private key")?;
        let mut key = None;
        for b in &blocks {
            let k = match b.label.as_str() {
                "PRIVATE KEY" => key_from_pkcs8(&b.der)?,
                "RSA PRIVATE KEY" => {
                    let k = rsa::RsaPrivateKey::from_pkcs1_der(&b.der)
                        .map_err(|e| format!("the RSA private key (PKCS#1) cannot be read: {e}"))?;
                    rsa_key(k)?
                }
                "EC PRIVATE KEY" => {
                    let sk = p256::SecretKey::from_sec1_der(&b.der).map_err(|_| {
                        "the EC private key (SEC1) cannot be read: only keys on the P-256 curve are supported"
                            .to_string()
                    })?;
                    Key::P256(p256::ecdsa::SigningKey::from(sk))
                }
                "ENCRYPTED PRIVATE KEY" => {
                    return Err("the private key is encrypted: give it unencrypted, or use the PKCS#12 file with its password".into())
                }
                _ => continue,
            };
            key = Some(k);
            break;
        }
        let key = key.ok_or(
            "no private key found: expected a PEM block `-----BEGIN PRIVATE KEY-----`, `-----BEGIN RSA PRIVATE KEY-----` or `-----BEGIN EC PRIVATE KEY-----`",
        )?;
        let certs: Vec<Vec<u8>> = pem::blocks(chain_pem, "the certificate chain")?
            .into_iter()
            .filter(|b| b.label == "CERTIFICATE")
            .map(|b| b.der)
            .collect();
        if certs.is_empty() {
            return Err("no certificate found: expected one or more PEM blocks `-----BEGIN CERTIFICATE-----`".into());
        }
        Signer::assemble(key, certs, "the certificate chain")
    }

    fn assemble(key: Key, cert_ders: Vec<Vec<u8>>, source: &str) -> Result<Signer, String> {
        let mut certs = Vec::new();
        for (i, d) in cert_ders.iter().enumerate() {
            let c = Certificate::from_der(d).map_err(|e| {
                format!(
                    "certificate number {} in {source} cannot be read: {e}",
                    i + 1
                )
            })?;
            certs.push(c);
        }
        if certs.is_empty() {
            return Err(format!("{source} holds no certificate"));
        }
        let Some(leaf) = certs.iter().position(|c| key_matches(&key, c)) else {
            let names: Vec<String> = certs
                .iter()
                .map(|c| format!("`{}`", display_name(&c.tbs_certificate.subject)))
                .collect();
            return Err(format!(
                "the private key does not match the certificate {} (it belongs to another certificate)",
                names.join(", ")
            ));
        };
        // Signer first, then follow the issuers.
        let mut order = vec![leaf];
        loop {
            let cur = &certs[*order.last().unwrap_or(&leaf)];
            if cur.tbs_certificate.issuer == cur.tbs_certificate.subject {
                break;
            }
            let next = (0..certs.len()).find(|&i| {
                !order.contains(&i)
                    && certs[i].tbs_certificate.subject == cur.tbs_certificate.issuer
            });
            match next {
                Some(i) => order.push(i),
                None => break,
            }
        }
        let chain: Vec<Certificate> = order.iter().map(|&i| certs[i].clone()).collect();
        let chain_der = order.iter().map(|&i| cert_ders[i].clone()).collect();
        Ok(Signer {
            key,
            chain,
            chain_der,
        })
    }

    /// Who the certificate belongs to, for messages: its common name, or else its
    /// organization, or else the whole distinguished name.
    pub fn subject(&self) -> String {
        display_name(&self.leaf().tbs_certificate.subject)
    }

    /// The certificate's subject distinguished name (RFC 4514).
    pub fn subject_dn(&self) -> String {
        self.leaf().tbs_certificate.subject.to_string()
    }

    /// The distinguished name of the certificate's issuer (RFC 4514).
    pub fn issuer_dn(&self) -> String {
        self.leaf().tbs_certificate.issuer.to_string()
    }

    /// When the certificate starts being valid.
    pub fn not_before(&self) -> SigningTime {
        cert_not_before(self.leaf())
    }

    /// When the certificate expires. Signing at a later time is refused.
    pub fn not_after(&self) -> SigningTime {
        cert_not_after(self.leaf())
    }

    /// The signer's certificate, DER encoded.
    pub fn certificate_der(&self) -> &[u8] {
        &self.chain_der[0]
    }

    /// The signer's certificate first, then its issuers, DER encoded.
    pub fn chain_der(&self) -> &[Vec<u8>] {
        &self.chain_der
    }

    pub(crate) fn leaf(&self) -> &Certificate {
        &self.chain[0]
    }

    /// Refuses a signing time outside the certificate's validity.
    pub(crate) fn check_valid_at(&self, time: SigningTime) -> Result<(), String> {
        check_cert_time(self.leaf(), time)
    }

    /// Signature as CMS wants it: PKCS#1 v1.5 for RSA, DER `ECDSA-Sig-Value` for ECDSA.
    pub(crate) fn sign_cms(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        match &self.key {
            Key::Rsa(k) => sign_rsa(k, message),
            Key::P256(k) => {
                let sig: p256::ecdsa::Signature = k.sign(message);
                Ok(sig.to_der().as_bytes().to_vec())
            }
        }
    }

    /// Signature as XML-DSig wants it: PKCS#1 v1.5 for RSA, `r || s` for ECDSA.
    pub(crate) fn sign_xmldsig(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        match &self.key {
            Key::Rsa(k) => sign_rsa(k, message),
            Key::P256(k) => {
                let sig: p256::ecdsa::Signature = k.sign(message);
                Ok(sig.to_bytes().to_vec())
            }
        }
    }

    /// The CMS `signatureAlgorithm`, DER encoded.
    pub(crate) fn cms_signature_algorithm(&self) -> Vec<u8> {
        match self.key {
            Key::Rsa(_) => {
                crate::asn1::algorithm(&oids::SHA256_WITH_RSA, Some(&crate::asn1::null()))
            }
            Key::P256(_) => crate::asn1::algorithm(&oids::ECDSA_WITH_SHA256, None),
        }
    }

    /// The XML-DSig `SignatureMethod` algorithm URI.
    pub(crate) fn xmldsig_method(&self) -> &'static str {
        match self.key {
            Key::Rsa(_) => crate::xades::RSA_SHA256,
            Key::P256(_) => crate::xades::ECDSA_SHA256,
        }
    }
}

fn sign_rsa(k: &rsa::RsaPrivateKey, message: &[u8]) -> Result<Vec<u8>, String> {
    let digest = Sha256::digest(message);
    // The random generator only blinds the private-key operation against timing
    // attacks: PKCS#1 v1.5 signatures are deterministic, the output does not depend on it.
    k.sign_with_rng(
        &mut rand::rngs::OsRng,
        Pkcs1v15Sign::new::<Sha256>(),
        &digest,
    )
    .map_err(|e| format!("RSA signing failed: {e}"))
}

fn rsa_key(k: rsa::RsaPrivateKey) -> Result<Key, String> {
    let bits = k.size() * 8;
    if bits < 2048 {
        return Err(format!("the RSA key has {bits} bits: keys shorter than 2048 bits are not accepted for signatures"));
    }
    Ok(Key::Rsa(Box::new(k)))
}

/// A private key from a PKCS#8 `PrivateKeyInfo`.
pub(crate) fn key_from_pkcs8(der: &[u8]) -> Result<Key, String> {
    let info = pkcs8::PrivateKeyInfo::try_from(der)
        .map_err(|e| format!("the private key (PKCS#8) cannot be read: {e}"))?;
    let alg = info.algorithm.oid;
    if alg == oids::RSA_ENCRYPTION {
        let k = rsa::RsaPrivateKey::from_pkcs8_der(der)
            .map_err(|e| format!("the RSA private key cannot be read: {e}"))?;
        rsa_key(k)
    } else if alg == oids::EC_PUBLIC_KEY {
        let curve = info.algorithm.parameters_oid().ok();
        if curve != Some(P256_CURVE) {
            let name = curve
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".into());
            return Err(format!(
                "the EC private key is on curve {name}: only P-256 (prime256v1) is supported"
            ));
        }
        let sk = p256::SecretKey::from_pkcs8_der(der)
            .map_err(|e| format!("the EC private key cannot be read: {e}"))?;
        Ok(Key::P256(p256::ecdsa::SigningKey::from(sk)))
    } else {
        Err(format!("unsupported private key type (algorithm {alg}): only RSA and ECDSA P-256 keys are supported"))
    }
}

fn key_matches(key: &Key, cert: &Certificate) -> bool {
    let Ok(spki) = cert.tbs_certificate.subject_public_key_info.to_der() else {
        return false;
    };
    match key {
        Key::Rsa(k) => rsa::RsaPublicKey::from_public_key_der(&spki)
            .map(|p| p.n() == k.n() && p.e() == k.e())
            .unwrap_or(false),
        Key::P256(k) => p256::PublicKey::from_public_key_der(&spki)
            .map(|p| p == p256::PublicKey::from(k.verifying_key()))
            .unwrap_or(false),
    }
}

// ------------------------------------------------------------- certificates

pub(crate) fn cert_not_before(c: &Certificate) -> SigningTime {
    SigningTime::from_unix_seconds(
        c.tbs_certificate
            .validity
            .not_before
            .to_unix_duration()
            .as_secs(),
    )
}

pub(crate) fn cert_not_after(c: &Certificate) -> SigningTime {
    SigningTime::from_unix_seconds(
        c.tbs_certificate
            .validity
            .not_after
            .to_unix_duration()
            .as_secs(),
    )
}

pub(crate) fn check_cert_time(c: &Certificate, time: SigningTime) -> Result<(), String> {
    let who = display_name(&c.tbs_certificate.subject);
    let (from, to) = (cert_not_before(c), cert_not_after(c));
    if time > to {
        return Err(format!(
            "the certificate of `{who}` expired on {} (signing time {time})",
            to.date()
        ));
    }
    if time < from {
        return Err(format!(
            "the certificate of `{who}` is valid only from {} (signing time {time})",
            from.date()
        ));
    }
    Ok(())
}

/// Common name, else organization, else the whole distinguished name.
pub(crate) fn display_name(name: &Name) -> String {
    attribute(name, &oids::COMMON_NAME)
        .or_else(|| attribute(name, &oids::ORGANIZATION))
        .unwrap_or_else(|| name.to_string())
}

fn attribute(name: &Name, oid: &der::oid::ObjectIdentifier) -> Option<String> {
    for rdn in name.0.iter() {
        for atv in rdn.0.iter() {
            if atv.oid == *oid {
                return Some(directory_string(atv.value.tag().octet(), atv.value.value()));
            }
        }
    }
    None
}

fn directory_string(tag: u8, bytes: &[u8]) -> String {
    match tag {
        // BMPString: UTF-16BE
        0x1e => {
            let units: Vec<u16> = bytes
                .chunks(2)
                .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        // TeletexString: treated as Latin-1, as most software does
        0x14 => bytes.iter().map(|&b| b as char).collect(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

pub(crate) fn serial_bytes(c: &Certificate) -> &[u8] {
    c.tbs_certificate.serial_number.as_bytes()
}

pub(crate) fn serial_hex(c: &Certificate) -> String {
    let b = serial_bytes(c);
    let b = if b.len() > 1 && b[0] == 0 { &b[1..] } else { b };
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// The serial number in decimal, as XML-DSig's `X509SerialNumber` wants it.
pub(crate) fn serial_decimal(c: &Certificate) -> String {
    let mut num: Vec<u8> = serial_bytes(c).to_vec();
    let mut digits = Vec::new();
    while num.iter().any(|&b| b != 0) {
        let mut rem = 0u32;
        for b in num.iter_mut() {
            let cur = (rem << 8) | *b as u32;
            *b = (cur / 10) as u8;
            rem = cur % 10;
        }
        digits.push(b'0' + rem as u8);
    }
    if digits.is_empty() {
        return "0".into();
    }
    digits.reverse();
    String::from_utf8(digits).unwrap_or_default()
}

/// How an ECDSA signature value is encoded.
#[derive(Clone, Copy)]
pub(crate) enum EcdsaFormat {
    /// DER `ECDSA-Sig-Value` (CMS).
    Der,
    /// `r || s` (XML-DSig).
    Raw,
}

/// Checks `signature` over `message` (SHA-256) with the certificate's public key.
pub(crate) fn verify_with_cert(
    cert: &Certificate,
    message: &[u8],
    signature: &[u8],
    fmt: EcdsaFormat,
) -> Result<(), String> {
    let who = display_name(&cert.tbs_certificate.subject);
    let spki = cert
        .tbs_certificate
        .subject_public_key_info
        .to_der()
        .map_err(|e| format!("the public key of `{who}` cannot be read: {e}"))?;
    let alg = cert.tbs_certificate.subject_public_key_info.algorithm.oid;
    let bad = || {
        format!("the signature does not match the data and the certificate of `{who}`: the signed content was changed, or it was signed with another key")
    };
    if alg == oids::RSA_ENCRYPTION {
        let pk = rsa::RsaPublicKey::from_public_key_der(&spki)
            .map_err(|e| format!("the RSA key of `{who}` cannot be read: {e}"))?;
        pk.verify(
            Pkcs1v15Sign::new::<Sha256>(),
            &Sha256::digest(message),
            signature,
        )
        .map_err(|_| bad())
    } else if alg == oids::EC_PUBLIC_KEY {
        let pk = p256::PublicKey::from_public_key_der(&spki).map_err(|_| {
            format!("the EC key of `{who}` is not on P-256, the only supported curve")
        })?;
        let vk = p256::ecdsa::VerifyingKey::from(pk);
        let sig = match fmt {
            EcdsaFormat::Der => p256::ecdsa::Signature::from_der(signature),
            EcdsaFormat::Raw => p256::ecdsa::Signature::from_slice(signature),
        }
        .map_err(|_| format!("the ECDSA signature value of `{who}` is malformed"))?;
        vk.verify(message, &sig).map_err(|_| bad())
    } else {
        Err(format!(
            "the certificate of `{who}` has an unsupported key type ({alg})"
        ))
    }
}

/// SHA-256 of a DER certificate.
pub(crate) fn cert_sha256(der: &[u8]) -> Vec<u8> {
    Sha256::digest(der).to_vec()
}

/// The certificate fields a verifier reports.
pub(crate) fn verified(
    signer: &Certificate,
    signer_der: Vec<u8>,
    others: Vec<Vec<u8>>,
    signing_time: Option<SigningTime>,
    covers_whole_file: bool,
) -> crate::Verified {
    let mut certificates = vec![signer_der.clone()];
    certificates.extend(others.into_iter().filter(|d| *d != signer_der));
    crate::Verified {
        subject: display_name(&signer.tbs_certificate.subject),
        subject_dn: signer.tbs_certificate.subject.to_string(),
        issuer_dn: signer.tbs_certificate.issuer.to_string(),
        serial_hex: serial_hex(signer),
        not_before: cert_not_before(signer),
        not_after: cert_not_after(signer),
        signing_time,
        certificates,
        covers_whole_file,
    }
}
