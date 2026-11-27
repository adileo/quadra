//! Digital signatures for documents: PAdES for PDF, CAdES (`.p7m`) and XAdES for
//! FatturaPA, and OpenPGP signing and encryption for banks' file exchange.
//!
//! Everything here is pure: bytes in, bytes out. There is no file or network access,
//! no clock (the signing time is always a parameter) and no global state. Keys and
//! certificates are passed in by the caller, who reads them from wherever secrets live.
//! The one thing taken from the operating system is randomness: for OpenPGP session
//! keys, and to blind RSA private-key operations against timing attacks (blinding does
//! not change the signature).
//!
//! With the same input, key and signing time, the X.509 based signatures ([`pades`],
//! [`cades_p7m`], [`xades_enveloped`]) and OpenPGP signatures made with RSA keys
//! ([`pgp_sign_detached`]) give the same bytes every time: RSA uses PKCS#1 v1.5 and
//! ECDSA uses deterministic nonces (RFC 6979). OpenPGP encryption ([`pgp_encrypt`]) is
//! not deterministic: every message gets a fresh random session key.
//!
//! Supported keys: RSA (2048 bits or more is advisable) and ECDSA on P-256. The digest
//! is always SHA-256.
//!
//! The verifiers ([`verify_pades`], [`verify_cades`], [`verify_xades`]) check that the
//! signed data is intact and that the signature was made with the private key of the
//! certificate carried in the signature. They do not decide whether that certificate is
//! trusted: [`Verified`] says who signed, and the caller checks the issuer.

mod asn1;
mod cades;
mod oids;
mod openpgp;
mod p12;
mod pades;
mod pdf;
mod pem;
mod signer;
mod xades;
mod xml;

pub use cades::{cades_p7m, verify_cades};
pub use openpgp::{pgp_encrypt, pgp_encrypt_armored, pgp_sign_detached};
pub use pades::{pades, verify_pades};
pub use signer::Signer;
pub use xades::{verify_xades, xades_enveloped};

/// Canonical XML 1.0 (inclusive, without comments) of a whole document: the form whose
/// SHA-256 an enveloped signature with `URI=""` signs.
///
/// Supports the XML described at [`xades_enveloped`]; anything else is refused with an
/// error that says what is not supported.
pub fn canonicalize_xml(xml_text: &str) -> Result<String, String> {
    let doc = xml::parse(xml_text)?;
    Ok(xml::c14n(&doc, doc.root, None))
}

/// A point in time, in whole seconds since 1970-01-01T00:00:00Z (UTC).
///
/// This is the signing time given to the signing functions, and the type of the dates
/// read from certificates. It is shown as `2026-06-01T12:00:00Z`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SigningTime(u64);

impl SigningTime {
    /// The time `seconds` after 1970-01-01T00:00:00Z.
    pub fn from_unix_seconds(seconds: u64) -> SigningTime {
        SigningTime(seconds)
    }

    /// Seconds since 1970-01-01T00:00:00Z.
    pub fn unix_seconds(self) -> u64 {
        self.0
    }

    /// The date alone, as `2026-01-31`.
    pub fn date(self) -> String {
        match self.datetime() {
            Ok(d) => format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day()),
            Err(_) => format!("(unix time {})", self.0),
        }
    }

    pub(crate) fn datetime(self) -> Result<der::DateTime, String> {
        der::DateTime::from_unix_duration(std::time::Duration::from_secs(self.0)).map_err(|_| {
            format!(
                "the time {} is out of the supported range (years 1970 to 9999)",
                self.0
            )
        })
    }

    pub(crate) fn from_datetime(d: &der::DateTime) -> SigningTime {
        SigningTime(d.unix_duration().as_secs())
    }
}

impl std::fmt::Display for SigningTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.datetime() {
            Ok(d) => write!(
                f,
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                d.year(),
                d.month(),
                d.day(),
                d.hour(),
                d.minutes(),
                d.seconds()
            ),
            Err(_) => write!(f, "(unix time {})", self.0),
        }
    }
}

/// Optional text shown by PDF readers in the signature panel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PadesInfo {
    /// Why the document was signed (`/Reason`), e.g. "Invoice issued".
    pub reason: Option<String>,
    /// Where it was signed (`/Location`), e.g. "Milano".
    pub location: Option<String>,
    /// The signer's name as shown (`/Name`). Readers otherwise show the certificate's.
    pub name: Option<String>,
}

/// What a verifier found in a valid signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// The signer: the certificate's common name, or the organization, or the whole
    /// distinguished name when it has neither.
    pub subject: String,
    /// The signer certificate's subject distinguished name (RFC 4514), e.g.
    /// `CN=ACME S.p.A.,O=ACME S.p.A.,C=IT`.
    pub subject_dn: String,
    /// The distinguished name of the certificate's issuer.
    pub issuer_dn: String,
    /// The certificate's serial number, in hexadecimal.
    pub serial_hex: String,
    /// When the signer certificate starts being valid.
    pub not_before: SigningTime,
    /// When the signer certificate stops being valid.
    pub not_after: SigningTime,
    /// The signing time claimed by the signer (CMS `signingTime`, PDF `/M`, XAdES
    /// `SigningTime`), when present. It is the signer's claim, not a trusted timestamp.
    pub signing_time: Option<SigningTime>,
    /// The signer certificate first, then the other certificates found in the signature,
    /// all DER encoded.
    pub certificates: Vec<Vec<u8>>,
    /// For PDF: whether the signature covers the whole file. False when content was
    /// appended after signing (a later incremental update), which the signature does not
    /// protect. Always true for CAdES and XAdES.
    pub covers_whole_file: bool,
}
