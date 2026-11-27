//! OpenPGP detached signatures and encryption (RFC 4880 / RFC 9580, v4 keys), for
//! the file exchange that banks often require.

use crate::SigningTime;
use pgp::composed::{
    ArmorOptions, Deserializable, DetachedSignature, MessageBuilder, SignedPublicKey,
    SignedSecretKey, SubpacketConfig,
};
use pgp::crypto::hash::HashAlgorithm;
use pgp::crypto::sym::SymmetricKeyAlgorithm;
use pgp::packet::{Subpacket, SubpacketData};
use pgp::types::{KeyDetails, Password, SigningKey, Timestamp};

/// Makes an ASCII-armored detached OpenPGP signature (`.asc`) of `data`.
///
/// `secret_key_armored` is an armored transferable secret key (as exported by
/// `gpg --armor --export-secret-keys`); the primary key signs when it may, otherwise a
/// signing subkey. `passphrase` unlocks it when it is protected. The signature is a
/// binary-mode v4 signature with SHA-256, made at `time`. With an RSA or Ed25519 key the
/// output is the same for the same data, key and time.
pub fn pgp_sign_detached(
    data: &[u8],
    secret_key_armored: &str,
    passphrase: Option<&str>,
    time: SigningTime,
) -> Result<String, String> {
    let key = read_secret_key(secret_key_armored)?;
    let signer = signing_key(&key)?;
    let pw = password(&key, passphrase)?;
    let sig = DetachedSignature::sign_binary_data_with_subpackets(
        rand::rngs::OsRng,
        &Box::new(signer),
        &pw,
        HashAlgorithm::Sha256,
        data,
        subpackets(signer, time)?,
    )
    .map_err(|e| unlock_error(&key, e))?;
    sig.to_armored_string(ArmorOptions::default())
        .map_err(|e| format!("cannot write the OpenPGP signature: {e}"))
}

/// Encrypts `data` for one or more recipients and returns a binary OpenPGP message
/// (`.pgp` / `.gpg`).
///
/// Each recipient is an armored public key (`gpg --armor --export`); its encryption
/// subkey is used. With `sign_with` (an armored secret key and its passphrase) the
/// message is also signed, at `time`. The message uses AES-256 in a SEIPD v1 packet
/// (integrity protected), which every current OpenPGP implementation reads.
///
/// The output is not deterministic: every message gets a fresh random session key,
/// taken from the operating system's random source.
pub fn pgp_encrypt(
    data: &[u8],
    recipients_armored: &[&str],
    sign_with: Option<(&str, Option<&str>)>,
    time: SigningTime,
) -> Result<Vec<u8>, String> {
    encrypt(data, recipients_armored, sign_with, time, false)
}

/// Like [`pgp_encrypt`], but returns the message ASCII-armored (`.asc`).
pub fn pgp_encrypt_armored(
    data: &[u8],
    recipients_armored: &[&str],
    sign_with: Option<(&str, Option<&str>)>,
    time: SigningTime,
) -> Result<String, String> {
    let bytes = encrypt(data, recipients_armored, sign_with, time, true)?;
    String::from_utf8(bytes)
        .map_err(|_| "internal error: the armored message is not text".to_string())
}

fn encrypt(
    data: &[u8],
    recipients_armored: &[&str],
    sign_with: Option<(&str, Option<&str>)>,
    time: SigningTime,
    armored: bool,
) -> Result<Vec<u8>, String> {
    if recipients_armored.is_empty() {
        return Err("no recipient: give at least one OpenPGP public key".into());
    }
    let recipients: Vec<SignedPublicKey> = recipients_armored
        .iter()
        .enumerate()
        .map(|(i, r)| {
            SignedPublicKey::from_string(r)
                .map(|(k, _)| k)
                .map_err(|e| format!("the public key of recipient {} cannot be read: {e}", i + 1))
        })
        .collect::<Result<_, _>>()?;
    let signing = match sign_with {
        Some((armored_key, pass)) => Some((read_secret_key(armored_key)?, pass)),
        None => None,
    };

    let rng = rand::rngs::OsRng;
    let mut builder =
        MessageBuilder::from_bytes("", data.to_vec()).seipd_v1(rng, SymmetricKeyAlgorithm::AES256);
    for (i, r) in recipients.iter().enumerate() {
        let who = user_id(r);
        let res = match r.public_subkeys.iter().find(|s| {
            s.key.algorithm().can_encrypt()
                && s.signatures
                    .iter()
                    .any(|sig| sig.key_flags().encrypt_comms() || sig.key_flags().encrypt_storage())
        }) {
            Some(sub) => builder.encrypt_to_key(rng, sub).map(|_| ()),
            None if r.primary_key.algorithm().can_encrypt() => {
                builder.encrypt_to_key(rng, r).map(|_| ())
            }
            None => {
                return Err(format!(
                    "the public key of recipient {} ({who}) has no encryption key",
                    i + 1
                ))
            }
        };
        res.map_err(|e| format!("cannot encrypt for recipient {} ({who}): {e}", i + 1))?;
    }
    let mut unlock: Option<&SignedSecretKey> = None;
    if let Some((key, pass)) = &signing {
        let signer = signing_key(key)?;
        let pw = password(key, *pass)?;
        builder.sign_with_subpackets(signer, pw, HashAlgorithm::Sha256, subpackets(signer, time)?);
        unlock = Some(key);
    }
    let result = if armored {
        builder
            .to_armored_string(rng, ArmorOptions::default())
            .map(String::into_bytes)
    } else {
        builder.to_vec(rng)
    };
    result.map_err(|e| match unlock {
        Some(k) => unlock_error(k, e),
        None => format!("OpenPGP encryption failed: {e}"),
    })
}

fn read_secret_key(armored: &str) -> Result<SignedSecretKey, String> {
    if armored.contains("BEGIN PGP PUBLIC KEY BLOCK") {
        return Err("this is an OpenPGP public key: signing needs the secret key (`gpg --armor --export-secret-keys`)".into());
    }
    SignedSecretKey::from_string(armored)
        .map(|(k, _)| k)
        .map_err(|e| format!("the OpenPGP secret key cannot be read: {e}"))
}

/// The primary key when it may sign, else the first signing subkey.
fn signing_key(k: &SignedSecretKey) -> Result<&dyn SigningKey, String> {
    let primary_flags = k
        .details
        .users
        .iter()
        .flat_map(|u| u.signatures.iter())
        .chain(k.details.direct_signatures.iter())
        .any(|s| s.key_flags().sign());
    if primary_flags && k.primary_key.algorithm().can_sign() {
        return Ok(&k.primary_key);
    }
    if let Some(sub) = k.secret_subkeys.iter().find(|s| {
        s.key.algorithm().can_sign() && s.signatures.iter().any(|sig| sig.key_flags().sign())
    }) {
        return Ok(&sub.key);
    }
    if k.primary_key.algorithm().can_sign() {
        return Ok(&k.primary_key);
    }
    Err("the OpenPGP secret key has no key that can sign".into())
}

fn password(key: &SignedSecretKey, passphrase: Option<&str>) -> Result<Password, String> {
    let encrypted = key.primary_key.secret_params().is_encrypted()
        || key
            .secret_subkeys
            .iter()
            .any(|s| s.key.secret_params().is_encrypted());
    match passphrase {
        Some(p) => Ok(Password::from(p)),
        None if encrypted => Err(format!(
            "the OpenPGP key of {} is protected by a passphrase: give the passphrase",
            secret_user_id(key)
        )),
        None => Ok(Password::empty()),
    }
}

fn unlock_error(key: &SignedSecretKey, e: pgp::errors::Error) -> String {
    let encrypted = key.primary_key.secret_params().is_encrypted()
        || key
            .secret_subkeys
            .iter()
            .any(|s| s.key.secret_params().is_encrypted());
    if encrypted {
        format!(
            "wrong passphrase for the OpenPGP key of {} (it cannot be unlocked: {e})",
            secret_user_id(key)
        )
    } else {
        format!("OpenPGP signing failed: {e}")
    }
}

fn subpackets(signer: &dyn SigningKey, time: SigningTime) -> Result<SubpacketConfig, String> {
    let secs = u32::try_from(time.unix_seconds()).map_err(|_| {
        format!("the signing time {time} is beyond what OpenPGP can represent (year 2106)")
    })?;
    if signer.created_at().as_secs() > secs {
        return Err(format!(
            "the OpenPGP key was created on {}, after the signing time {time}",
            SigningTime::from_unix_seconds(signer.created_at().as_secs() as u64)
        ));
    }
    let sp =
        |d| Subpacket::regular(d).map_err(|e| format!("cannot build the OpenPGP signature: {e}"));
    Ok(SubpacketConfig::UserDefined {
        hashed: vec![
            sp(SubpacketData::SignatureCreationTime(Timestamp::from_secs(
                secs,
            )))?,
            sp(SubpacketData::IssuerFingerprint(signer.fingerprint()))?,
        ],
        unhashed: vec![sp(SubpacketData::IssuerKeyId(signer.legacy_key_id()))?],
    })
}

fn user_id(k: &SignedPublicKey) -> String {
    k.details
        .users
        .first()
        .map(|u| format!("`{}`", String::from_utf8_lossy(u.id.id())))
        .unwrap_or_else(|| format!("key {}", k.primary_key.fingerprint()))
}

fn secret_user_id(k: &SignedSecretKey) -> String {
    k.details
        .users
        .first()
        .map(|u| format!("`{}`", String::from_utf8_lossy(u.id.id())))
        .unwrap_or_else(|| format!("key {}", k.primary_key.fingerprint()))
}
