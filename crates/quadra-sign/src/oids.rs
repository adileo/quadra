//! Object identifiers used by this crate.

use der::oid::ObjectIdentifier as Oid;

pub(crate) const DATA: Oid = Oid::new_unwrap("1.2.840.113549.1.7.1");
pub(crate) const SIGNED_DATA: Oid = Oid::new_unwrap("1.2.840.113549.1.7.2");
pub(crate) const ENCRYPTED_DATA: Oid = Oid::new_unwrap("1.2.840.113549.1.7.6");

pub(crate) const CONTENT_TYPE: Oid = Oid::new_unwrap("1.2.840.113549.1.9.3");
pub(crate) const MESSAGE_DIGEST: Oid = Oid::new_unwrap("1.2.840.113549.1.9.4");
pub(crate) const SIGNING_TIME: Oid = Oid::new_unwrap("1.2.840.113549.1.9.5");
pub(crate) const SIGNING_CERTIFICATE: Oid = Oid::new_unwrap("1.2.840.113549.1.9.16.2.12");
pub(crate) const SIGNING_CERTIFICATE_V2: Oid = Oid::new_unwrap("1.2.840.113549.1.9.16.2.47");

pub(crate) const SHA1: Oid = Oid::new_unwrap("1.3.14.3.2.26");
pub(crate) const SHA256: Oid = Oid::new_unwrap("2.16.840.1.101.3.4.2.1");
pub(crate) const SHA384: Oid = Oid::new_unwrap("2.16.840.1.101.3.4.2.2");
pub(crate) const SHA512: Oid = Oid::new_unwrap("2.16.840.1.101.3.4.2.3");

pub(crate) const RSA_ENCRYPTION: Oid = Oid::new_unwrap("1.2.840.113549.1.1.1");
pub(crate) const SHA256_WITH_RSA: Oid = Oid::new_unwrap("1.2.840.113549.1.1.11");
pub(crate) const EC_PUBLIC_KEY: Oid = Oid::new_unwrap("1.2.840.10045.2.1");
pub(crate) const ECDSA_WITH_SHA256: Oid = Oid::new_unwrap("1.2.840.10045.4.3.2");

// PKCS#12
pub(crate) const PKCS8_SHROUDED_KEY_BAG: Oid = Oid::new_unwrap("1.2.840.113549.1.12.10.1.2");
pub(crate) const KEY_BAG: Oid = Oid::new_unwrap("1.2.840.113549.1.12.10.1.1");
pub(crate) const CERT_BAG: Oid = Oid::new_unwrap("1.2.840.113549.1.12.10.1.3");
pub(crate) const X509_CERTIFICATE: Oid = Oid::new_unwrap("1.2.840.113549.1.9.22.1");
pub(crate) const PBE_SHA1_RC4_128: Oid = Oid::new_unwrap("1.2.840.113549.1.12.1.1");
pub(crate) const PBE_SHA1_RC4_40: Oid = Oid::new_unwrap("1.2.840.113549.1.12.1.2");
pub(crate) const PBE_SHA1_3DES: Oid = Oid::new_unwrap("1.2.840.113549.1.12.1.3");
pub(crate) const PBE_SHA1_2DES: Oid = Oid::new_unwrap("1.2.840.113549.1.12.1.4");
pub(crate) const PBE_SHA1_RC2_128: Oid = Oid::new_unwrap("1.2.840.113549.1.12.1.5");
pub(crate) const PBE_SHA1_RC2_40: Oid = Oid::new_unwrap("1.2.840.113549.1.12.1.6");
pub(crate) const PBES2: Oid = Oid::new_unwrap("1.2.840.113549.1.5.13");
pub(crate) const PBKDF2: Oid = Oid::new_unwrap("1.2.840.113549.1.5.12");
pub(crate) const PBMAC1: Oid = Oid::new_unwrap("1.2.840.113549.1.5.14");
pub(crate) const HMAC_SHA1: Oid = Oid::new_unwrap("1.2.840.113549.2.7");
pub(crate) const HMAC_SHA256: Oid = Oid::new_unwrap("1.2.840.113549.2.9");
pub(crate) const HMAC_SHA384: Oid = Oid::new_unwrap("1.2.840.113549.2.10");
pub(crate) const HMAC_SHA512: Oid = Oid::new_unwrap("1.2.840.113549.2.11");

// X.500 attribute types
pub(crate) const COMMON_NAME: Oid = Oid::new_unwrap("2.5.4.3");
pub(crate) const ORGANIZATION: Oid = Oid::new_unwrap("2.5.4.10");
