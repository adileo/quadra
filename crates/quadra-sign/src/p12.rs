//! PKCS#12 (RFC 7292) reading: the private key and the certificates.
//!
//! Supported protections: PBES2 (PBKDF2 with AES-CBC or 3DES, the OpenSSL 3+ default),
//! the legacy PKCS#12 PBE algorithms (3DES, 2-key 3DES, RC2-128, RC2-40), the classic
//! HMAC integrity check (SHA-1/256/384/512) and PBMAC1 (RFC 9579). RC4 is refused.

use crate::asn1::{self, Tlv};
use crate::oids;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, InnerIvInit, KeyInit, KeyIvInit};
use der::Decode;
use hmac::Mac;
use sha2::Digest;

pub(crate) struct Contents {
    pub key_pkcs8: Vec<u8>,
    pub certificates: Vec<Vec<u8>>,
}

const WRONG_PASSWORD: &str = "wrong password for the PKCS#12 file (its integrity check failed)";

pub(crate) fn parse(p12: &[u8], password: &str) -> Result<Contents, String> {
    let not_p12 = |e: String| format!("this is not a PKCS#12 file: {e}");
    let pfx = asn1::read_whole(p12).map_err(not_p12)?;
    let parts = pfx.expect(asn1::SEQUENCE, "PFX").map_err(not_p12)?;
    if parts.len() < 2 || parts[0].small_uint() != Ok(3) {
        return Err(not_p12("unexpected structure or version".into()));
    }
    let auth_safe = parts[1]
        .expect(asn1::SEQUENCE, "authSafe")
        .map_err(not_p12)?;
    if auth_safe.is_empty() || !auth_safe[0].is_oid(&oids::DATA) {
        return Err(
            "PKCS#12 files protected with a public key instead of a password are not supported"
                .into(),
        );
    }
    let content = explicit_inner(auth_safe.get(1), "authSafe")?
        .octets()?
        .into_owned();
    // Without an integrity check, a wrong password shows up when decrypting.
    if let Some(mac) = parts.get(2) {
        verify_mac(mac, &content, password)?;
    }

    let mut out = Contents {
        key_pkcs8: Vec::new(),
        certificates: Vec::new(),
    };
    let mut keys = Vec::new();
    let safes = asn1::read_whole(&content)?.expect(asn1::SEQUENCE, "AuthenticatedSafe")?;
    for ci in safes {
        let kids = ci.expect(asn1::SEQUENCE, "ContentInfo")?;
        let ct = kids.first().ok_or("empty ContentInfo")?;
        if ct.is_oid(&oids::DATA) {
            let data = explicit_inner(kids.get(1), "data")?.octets()?.into_owned();
            read_bags(&data, password, &mut keys, &mut out.certificates, 0)?;
        } else if ct.is_oid(&oids::ENCRYPTED_DATA) {
            let ed = explicit_inner(kids.get(1), "encryptedData")?
                .expect(asn1::SEQUENCE, "EncryptedData")?;
            let eci = ed
                .get(1)
                .ok_or("EncryptedData without content")?
                .expect(asn1::SEQUENCE, "EncryptedContentInfo")?;
            let alg = eci.get(1).ok_or("EncryptedContentInfo without algorithm")?;
            let enc = eci.get(2).ok_or("EncryptedContentInfo without content")?;
            let bytes = match enc.tag {
                0x80 => enc.content.to_vec(),
                0xa0 => {
                    let mut v = Vec::new();
                    for p in enc.children()? {
                        v.extend_from_slice(&p.octets()?);
                    }
                    v
                }
                t => return Err(format!("unexpected encryptedContent tag 0x{t:02x}")),
            };
            let plain = decrypt(alg, &bytes, password)?;
            read_bags(&plain, password, &mut keys, &mut out.certificates, 0)?;
        } else {
            return Err(format!(
                "unsupported PKCS#12 content type {}",
                ct.oid_string()
            ));
        }
    }
    match keys.len() {
        0 => Err("the PKCS#12 file holds no private key".into()),
        _ => {
            out.key_pkcs8 = keys.swap_remove(0);
            Ok(out)
        }
    }
}

/// The element inside `[0] EXPLICIT`.
fn explicit_inner<'a>(t: Option<&Tlv<'a>>, what: &str) -> Result<Tlv<'a>, String> {
    let t = t.ok_or_else(|| format!("{what}: missing content"))?;
    let kids = t.expect(0xa0, what)?;
    kids.into_iter()
        .next()
        .ok_or_else(|| format!("{what}: empty content"))
}

fn read_bags(
    data: &[u8],
    password: &str,
    keys: &mut Vec<Vec<u8>>,
    certs: &mut Vec<Vec<u8>>,
    depth: usize,
) -> Result<(), String> {
    if depth > 4 {
        return Err("PKCS#12 bags nested too deep".into());
    }
    for bag in asn1::read_whole(data)?.expect(asn1::SEQUENCE, "SafeContents")? {
        let kids = bag.expect(asn1::SEQUENCE, "SafeBag")?;
        let id = kids.first().ok_or("empty SafeBag")?;
        let value = explicit_inner(kids.get(1), "SafeBag value")?;
        if id.is_oid(&oids::KEY_BAG) {
            keys.push(value.raw.to_vec());
        } else if id.is_oid(&oids::PKCS8_SHROUDED_KEY_BAG) {
            let epki = value.expect(asn1::SEQUENCE, "EncryptedPrivateKeyInfo")?;
            let alg = epki
                .first()
                .ok_or("EncryptedPrivateKeyInfo without algorithm")?;
            let enc = epki
                .get(1)
                .ok_or("EncryptedPrivateKeyInfo without data")?
                .octets()?;
            let key = decrypt(alg, &enc, password)?;
            // The decrypted key must parse: a wrong password without a MAC shows here.
            asn1::read_whole(&key).map_err(|_| WRONG_PASSWORD.to_string())?;
            keys.push(key);
        } else if id.is_oid(&oids::CERT_BAG) {
            let cb = value.expect(asn1::SEQUENCE, "CertBag")?;
            if cb
                .first()
                .is_some_and(|o| o.is_oid(&oids::X509_CERTIFICATE))
            {
                certs.push(
                    explicit_inner(cb.get(1), "CertBag value")?
                        .octets()?
                        .into_owned(),
                );
            }
        } else if id.oid_string() == "1.2.840.113549.1.12.10.1.6" {
            // safeContentsBag: nested bags.
            read_bags(value.raw, password, keys, certs, depth + 1)?;
        }
        // CRL and secret bags are ignored.
    }
    Ok(())
}

// ------------------------------------------------------------- hashes

#[derive(Clone, Copy)]
enum Hash {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    fn from_digest_oid(t: &Tlv) -> Option<Hash> {
        [
            (oids::SHA1, Hash::Sha1),
            (oids::SHA256, Hash::Sha256),
            (oids::SHA384, Hash::Sha384),
            (oids::SHA512, Hash::Sha512),
        ]
        .into_iter()
        .find(|(o, _)| t.is_oid(o))
        .map(|(_, h)| h)
    }

    fn from_hmac_oid(t: &Tlv) -> Option<Hash> {
        [
            (oids::HMAC_SHA1, Hash::Sha1),
            (oids::HMAC_SHA256, Hash::Sha256),
            (oids::HMAC_SHA384, Hash::Sha384),
            (oids::HMAC_SHA512, Hash::Sha512),
        ]
        .into_iter()
        .find(|(o, _)| t.is_oid(o))
        .map(|(_, h)| h)
    }

    fn len(self) -> usize {
        match self {
            Hash::Sha1 => 20,
            Hash::Sha256 => 32,
            Hash::Sha384 => 48,
            Hash::Sha512 => 64,
        }
    }

    fn block(self) -> usize {
        match self {
            Hash::Sha1 | Hash::Sha256 => 64,
            Hash::Sha384 | Hash::Sha512 => 128,
        }
    }

    fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Hash::Sha1 => sha1::Sha1::digest(data).to_vec(),
            Hash::Sha256 => sha2::Sha256::digest(data).to_vec(),
            Hash::Sha384 => sha2::Sha384::digest(data).to_vec(),
            Hash::Sha512 => sha2::Sha512::digest(data).to_vec(),
        }
    }

    /// Constant-time check of an HMAC.
    fn hmac_verify(self, key: &[u8], data: &[u8], expected: &[u8]) -> bool {
        macro_rules! check {
            ($d:ty) => {{
                let Ok(mut m) = <hmac::Hmac<$d> as Mac>::new_from_slice(key) else {
                    return false;
                };
                m.update(data);
                m.verify_slice(expected).is_ok()
            }};
        }
        match self {
            Hash::Sha1 => check!(sha1::Sha1),
            Hash::Sha256 => check!(sha2::Sha256),
            Hash::Sha384 => check!(sha2::Sha384),
            Hash::Sha512 => check!(sha2::Sha512),
        }
    }

    fn pbkdf2(self, password: &[u8], salt: &[u8], rounds: u32, out: &mut [u8]) {
        match self {
            Hash::Sha1 => pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, salt, rounds, out),
            Hash::Sha256 => pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password, salt, rounds, out),
            Hash::Sha384 => pbkdf2::pbkdf2_hmac::<sha2::Sha384>(password, salt, rounds, out),
            Hash::Sha512 => pbkdf2::pbkdf2_hmac::<sha2::Sha512>(password, salt, rounds, out),
        }
    }
}

/// The PKCS#12 key derivation function (RFC 7292 appendix B.2). `id`: 1 key, 2 IV, 3 MAC.
fn pkcs12_kdf(h: Hash, password: &str, salt: &[u8], id: u8, rounds: u64, n: usize) -> Vec<u8> {
    let v = h.block();
    // The password as a NUL-terminated BMPString (UTF-16BE).
    let mut pass: Vec<u8> = password
        .encode_utf16()
        .flat_map(|u| u.to_be_bytes())
        .collect();
    pass.extend_from_slice(&[0, 0]);
    let fill = |src: &[u8]| -> Vec<u8> {
        if src.is_empty() {
            return Vec::new();
        }
        let len = v * src.len().div_ceil(v);
        (0..len).map(|i| src[i % src.len()]).collect()
    };
    let mut i_buf = fill(salt);
    i_buf.extend(fill(&pass));
    let d = vec![id; v];
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let mut a = h.digest(&[d.as_slice(), &i_buf].concat());
        for _ in 1..rounds.max(1) {
            a = h.digest(&a);
        }
        let b: Vec<u8> = (0..v).map(|k| a[k % a.len()]).collect();
        for block in i_buf.chunks_mut(v) {
            // block = (block + b + 1) mod 2^(8v)
            let mut carry = 1u16;
            for k in (0..v).rev() {
                let s = block[k] as u16 + b[k] as u16 + carry;
                block[k] = s as u8;
                carry = s >> 8;
            }
        }
        out.extend_from_slice(&a);
    }
    out.truncate(n);
    out
}

fn verify_mac(mac: &Tlv, content: &[u8], password: &str) -> Result<(), String> {
    let md = mac.expect(asn1::SEQUENCE, "MacData")?;
    let di = md
        .first()
        .ok_or("MacData without DigestInfo")?
        .expect(asn1::SEQUENCE, "DigestInfo")?;
    let alg = di
        .first()
        .ok_or("DigestInfo without algorithm")?
        .expect(asn1::SEQUENCE, "AlgorithmIdentifier")?;
    let expected = di.get(1).ok_or("DigestInfo without digest")?.octets()?;
    let alg_oid = alg.first().ok_or("empty AlgorithmIdentifier")?;
    if alg_oid.is_oid(&oids::PBMAC1) {
        let params = alg
            .get(1)
            .ok_or("PBMAC1 without parameters")?
            .expect(asn1::SEQUENCE, "PBMAC1-params")?;
        let kdf = params
            .first()
            .ok_or("PBMAC1 without key derivation")?
            .expect(asn1::SEQUENCE, "PBMAC1 KDF")?;
        if !kdf.first().is_some_and(|o| o.is_oid(&oids::PBKDF2)) {
            return Err("PBMAC1 with a key derivation other than PBKDF2 is not supported".into());
        }
        let p = kdf
            .get(1)
            .ok_or("PBKDF2 without parameters")?
            .expect(asn1::SEQUENCE, "PBKDF2-params")?;
        let salt = p.first().ok_or("PBKDF2 without salt")?.octets()?;
        let rounds = p
            .get(1)
            .ok_or("PBKDF2 without iteration count")?
            .small_uint()?;
        let mut key_len = None;
        let mut prf = Hash::Sha1;
        for extra in &p[2..] {
            if extra.tag == asn1::INTEGER {
                key_len = Some(extra.small_uint()? as usize);
            } else if extra.tag == asn1::SEQUENCE {
                let a = extra.children()?;
                prf = a
                    .first()
                    .and_then(Hash::from_hmac_oid)
                    .ok_or("unsupported PBKDF2 pseudo-random function")?;
            }
        }
        let scheme = params
            .get(1)
            .ok_or("PBMAC1 without MAC scheme")?
            .expect(asn1::SEQUENCE, "PBMAC1 MAC")?;
        let mac_hash = scheme
            .first()
            .and_then(Hash::from_hmac_oid)
            .ok_or("unsupported PBMAC1 MAC algorithm")?;
        let key_len = key_len.ok_or("PBMAC1 without key length")?;
        if key_len == 0 || key_len > 128 || rounds > 10_000_000 {
            return Err("PBMAC1 parameters out of range".into());
        }
        let mut key = vec![0u8; key_len];
        prf.pbkdf2(password.as_bytes(), &salt, rounds as u32, &mut key);
        return if mac_hash.hmac_verify(&key, content, &expected) {
            Ok(())
        } else {
            Err(WRONG_PASSWORD.into())
        };
    }
    let h = Hash::from_digest_oid(alg_oid)
        .ok_or_else(|| format!("unsupported PKCS#12 MAC algorithm {}", alg_oid.oid_string()))?;
    let salt = md.get(1).ok_or("MacData without salt")?.octets()?;
    let rounds = match md.get(2) {
        Some(i) => i.small_uint()?,
        None => 1,
    };
    if rounds > 10_000_000 {
        return Err("PKCS#12 MAC iteration count out of range".into());
    }
    let key = pkcs12_kdf(h, password, &salt, 3, rounds, h.len());
    if h.hmac_verify(&key, content, &expected) {
        Ok(())
    } else {
        Err(WRONG_PASSWORD.into())
    }
}

// ------------------------------------------------------------- decryption

fn decrypt(alg: &Tlv, data: &[u8], password: &str) -> Result<Vec<u8>, String> {
    let a = alg.expect(asn1::SEQUENCE, "encryption algorithm")?;
    let oid = a.first().ok_or("empty encryption algorithm")?;
    let wrong = |_| WRONG_PASSWORD.to_string();
    if oid.is_oid(&oids::PBES2) {
        let raw = a.get(1).ok_or("PBES2 without parameters")?.raw;
        let params = pkcs5::pbes2::Parameters::from_der(raw)
            .map_err(|e| format!("unsupported PBES2 parameters in the PKCS#12 file: {e}"))?;
        return params
            .decrypt(password.as_bytes(), data)
            .map_err(|e| match e {
                pkcs5::Error::DecryptFailed => WRONG_PASSWORD.to_string(),
                e => format!("the PKCS#12 file cannot be decrypted: {e}"),
            });
    }
    let params = a
        .get(1)
        .ok_or("PBE without parameters")?
        .expect(asn1::SEQUENCE, "PBE parameters")?;
    let salt = params.first().ok_or("PBE without salt")?.octets()?;
    let rounds = params
        .get(1)
        .ok_or("PBE without iteration count")?
        .small_uint()?;
    if rounds > 10_000_000 {
        return Err("PKCS#12 iteration count out of range".into());
    }
    let key_iv = |len: usize| {
        (
            pkcs12_kdf(Hash::Sha1, password, &salt, 1, rounds, len),
            pkcs12_kdf(Hash::Sha1, password, &salt, 2, rounds, 8),
        )
    };
    let bad_len = |_| "bad key length".to_string();
    if oid.is_oid(&oids::PBE_SHA1_3DES) {
        let (k, iv) = key_iv(24);
        cbc::Decryptor::<des::TdesEde3>::new_from_slices(&k, &iv)
            .map_err(bad_len)?
            .decrypt_padded_vec_mut::<Pkcs7>(data)
            .map_err(wrong)
    } else if oid.is_oid(&oids::PBE_SHA1_2DES) {
        let (k, iv) = key_iv(16);
        cbc::Decryptor::<des::TdesEde2>::new_from_slices(&k, &iv)
            .map_err(bad_len)?
            .decrypt_padded_vec_mut::<Pkcs7>(data)
            .map_err(wrong)
    } else if oid.is_oid(&oids::PBE_SHA1_RC2_128) || oid.is_oid(&oids::PBE_SHA1_RC2_40) {
        let bits = if oid.is_oid(&oids::PBE_SHA1_RC2_40) {
            40
        } else {
            128
        };
        let (k, iv) = key_iv(bits / 8);
        let cipher = if bits == 128 {
            rc2::Rc2::new_from_slice(&k).map_err(bad_len)?
        } else {
            rc2::Rc2::new_with_eff_key_len(&k, bits)
        };
        cbc::Decryptor::<rc2::Rc2>::inner_iv_slice_init(cipher, &iv)
            .map_err(bad_len)?
            .decrypt_padded_vec_mut::<Pkcs7>(data)
            .map_err(wrong)
    } else if oid.is_oid(&oids::PBE_SHA1_RC4_128) || oid.is_oid(&oids::PBE_SHA1_RC4_40) {
        Err("the PKCS#12 file uses RC4, which is not supported: export it again with a current tool (e.g. `openssl pkcs12 -export`)".into())
    } else {
        Err(format!(
            "unsupported PKCS#12 encryption algorithm {}",
            oid.oid_string()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kdf_matches_known_vector() {
        // From the PKCS#12 test vectors (password "smeg", SHA-1, id 1, 1 round).
        let salt = [0x0A, 0x58, 0xCF, 0x64, 0x53, 0x0D, 0x82, 0x3F];
        let k = pkcs12_kdf(Hash::Sha1, "smeg", &salt, 1, 1, 24);
        let hex: String = k.iter().map(|b| format!("{b:02X}")).collect();
        assert_eq!(hex, "8AAAE6297B6CB04642AB5B077851284EB7128F1A2A7FBCA3");
    }
}
