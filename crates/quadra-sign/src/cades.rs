//! CMS SignedData (RFC 5652) with CAdES-BES signed attributes: the `.p7m` envelope,
//! and the detached signature inside a PAdES PDF.

use crate::asn1::{self, Tlv};
use crate::signer::{self, EcdsaFormat, Signer};
use crate::{oids, SigningTime, Verified};
use der::{Decode, Encode};
use sha2::{Digest, Sha256};
use x509_cert::Certificate;

/// Signs `content` as CAdES-BES and returns the enveloping (attached) CMS SignedData,
/// DER encoded: the content of a FatturaPA `.xml.p7m` file.
///
/// The signed attributes are contentType, signingTime (`time`), messageDigest
/// (SHA-256) and signing-certificate-v2; the whole certificate chain is included.
/// The signing time must fall within the certificate's validity. The output is the
/// same for the same content, key and time.
pub fn cades_p7m(content: &[u8], signer: &Signer, time: SigningTime) -> Result<Vec<u8>, String> {
    signer.check_valid_at(time)?;
    let digest = Sha256::digest(content);
    signed_data(signer, &digest, Some(content), Some(time))
}

/// Verifies a `.p7m` (CMS SignedData with the content inside) and returns the content.
///
/// Accepts DER or BER, and also the same in base64 or PEM (`-----BEGIN PKCS7-----`),
/// as some systems deliver it. Every signer in the file is checked; the result
/// describes the first. The certificate's trust is not checked: see [`Verified`].
pub fn verify_cades(p7m: &[u8]) -> Result<(Vec<u8>, Verified), String> {
    let decoded;
    let der = if p7m.first() == Some(&0x30) {
        p7m
    } else {
        let text = std::str::from_utf8(p7m)
            .map_err(|_| "this is not a CMS/PKCS#7 signed file (.p7m)".to_string())?;
        let body: String = text.lines().filter(|l| !l.starts_with("-----")).collect();
        decoded = crate::pem::b64_decode(&body)
            .map_err(|_| "this is not a CMS/PKCS#7 signed file (.p7m)".to_string())?;
        &decoded
    };
    let (content, verified) = verify_signed_data(der, None)?;
    Ok((content, verified))
}

/// Builds a DER ContentInfo holding SignedData.
///
/// `content`: `Some` for an enveloping signature (content inside), `None` for a
/// detached one. `time`: the signingTime attribute, left out for PAdES.
pub(crate) fn signed_data(
    signer: &Signer,
    content_digest: &[u8],
    content: Option<&[u8]>,
    time: Option<SigningTime>,
) -> Result<Vec<u8>, String> {
    let leaf = signer.leaf();
    let issuer = leaf
        .tbs_certificate
        .issuer
        .to_der()
        .map_err(|e| format!("cannot encode the certificate issuer: {e}"))?;
    let serial = asn1::tlv(asn1::INTEGER, signer::serial_bytes(leaf));
    let sha256 = asn1::algorithm(&oids::SHA256, None);

    let mut attrs = vec![
        attribute(&oids::CONTENT_TYPE, &asn1::oid(&oids::DATA)),
        attribute(&oids::MESSAGE_DIGEST, &asn1::octet_string(content_digest)),
    ];
    if let Some(t) = time {
        attrs.push(attribute(&oids::SIGNING_TIME, &encode_time(t)?));
    }
    // ESSCertIDv2: hashAlgorithm is left out because SHA-256 is its default value.
    let cert_hash = signer::cert_sha256(signer.certificate_der());
    let general_names = asn1::seq(&[&asn1::tlv(0xa4, &issuer)]);
    let issuer_serial = asn1::seq(&[&general_names, &serial]);
    let ess_cert_id = asn1::seq(&[&asn1::octet_string(&cert_hash), &issuer_serial]);
    let signing_cert_v2 = asn1::seq(&[&asn1::seq(&[&ess_cert_id])]);
    attrs.push(attribute(&oids::SIGNING_CERTIFICATE_V2, &signing_cert_v2));

    let signed_attrs = asn1::set_of(attrs);
    let signature = signer.sign_cms(&signed_attrs)?;

    let signer_info = asn1::seq(&[
        &asn1::small_int(1),
        &asn1::seq(&[&issuer, &serial]),
        &sha256,
        &asn1::implicit_constructed(0, &signed_attrs),
        &signer.cms_signature_algorithm(),
        &asn1::octet_string(&signature),
    ]);
    let encap = match content {
        Some(c) => asn1::seq(&[
            &asn1::oid(&oids::DATA),
            &asn1::explicit(0, &asn1::octet_string(c)),
        ]),
        None => asn1::seq(&[&asn1::oid(&oids::DATA)]),
    };
    let mut certs = signer.chain_der().to_vec();
    certs.sort();
    certs.dedup();
    let certificates = asn1::tlv(0xa0, &certs.concat());
    let sd = asn1::seq(&[
        &asn1::small_int(1),
        &asn1::set_of(vec![sha256.clone()]),
        &encap,
        &certificates,
        &asn1::set_of(vec![signer_info]),
    ]);
    Ok(asn1::seq(&[
        &asn1::oid(&oids::SIGNED_DATA),
        &asn1::explicit(0, &sd),
    ]))
}

fn attribute(oid: &der::oid::ObjectIdentifier, value: &[u8]) -> Vec<u8> {
    asn1::seq(&[&asn1::oid(oid), &asn1::set_of(vec![value.to_vec()])])
}

/// UTCTime for 1950-2049, GeneralizedTime otherwise (RFC 5652 11.3).
fn encode_time(t: SigningTime) -> Result<Vec<u8>, String> {
    let dt = t.datetime()?;
    let r = if (1950..2050).contains(&dt.year()) {
        der::asn1::UtcTime::from_date_time(dt).and_then(|u| u.to_der())
    } else {
        der::asn1::GeneralizedTime::from_date_time(dt).to_der()
    };
    r.map_err(|e| format!("cannot encode the signing time: {e}"))
}

fn decode_time(t: &Tlv) -> Result<SigningTime, String> {
    let dt = match t.tag {
        asn1::UTC_TIME => der::asn1::UtcTime::from_der(t.raw).map(|u| u.to_date_time()),
        asn1::GENERALIZED_TIME => {
            der::asn1::GeneralizedTime::from_der(t.raw).map(|g| g.to_date_time())
        }
        _ => return Err("the signingTime attribute is not a time".into()),
    }
    .map_err(|e| format!("the signingTime attribute cannot be read: {e}"))?;
    Ok(SigningTime::from_datetime(&dt))
}

/// Verifies a ContentInfo with SignedData. With `detached`, the content is given
/// separately (PAdES) and must not also be inside.
pub(crate) fn verify_signed_data(
    cms: &[u8],
    detached: Option<&[u8]>,
) -> Result<(Vec<u8>, Verified), String> {
    let not_cms = |e: String| format!("the CMS signature cannot be read: {e}");
    let ci = asn1::read_whole(cms).map_err(not_cms)?;
    let ci = ci.expect(asn1::SEQUENCE, "ContentInfo").map_err(not_cms)?;
    if ci.len() < 2 || !ci[0].is_oid(&oids::SIGNED_DATA) {
        return Err("this is not a CMS SignedData structure (a .p7m or a PDF signature)".into());
    }
    let sd = ci[1].expect(0xa0, "content").map_err(not_cms)?;
    let sd = sd.first().ok_or_else(|| not_cms("empty content".into()))?;
    let parts = sd.expect(asn1::SEQUENCE, "SignedData").map_err(not_cms)?;
    if parts.len() < 4 {
        return Err(not_cms("SignedData is incomplete".into()));
    }
    let encap = parts[2]
        .expect(asn1::SEQUENCE, "EncapsulatedContentInfo")
        .map_err(not_cms)?;
    let inner = match encap.get(1) {
        Some(e) => {
            let o = e.expect(0xa0, "eContent").map_err(not_cms)?;
            Some(
                o.first()
                    .ok_or_else(|| not_cms("empty eContent".into()))?
                    .octets()?
                    .into_owned(),
            )
        }
        None => None,
    };
    let content = match (detached, inner) {
        (Some(_), Some(_)) => {
            return Err(
                "the PDF signature carries content inside: a detached signature was expected"
                    .into(),
            )
        }
        (Some(d), None) => d.to_vec(),
        (None, Some(c)) => c,
        (None, None) => {
            return Err(
                "the CMS signature is detached: it does not contain the signed content".into(),
            )
        }
    };

    let mut certs: Vec<(Certificate, Vec<u8>)> = Vec::new();
    let mut signer_infos = None;
    for p in &parts[3..] {
        match p.tag {
            0xa0 => {
                for c in p.children()? {
                    if c.tag == asn1::SEQUENCE {
                        if let Ok(cert) = Certificate::from_der(c.raw) {
                            certs.push((cert, c.raw.to_vec()));
                        }
                    }
                }
            }
            0xa1 => {} // CRLs
            asn1::SET => signer_infos = Some(p.children()?),
            _ => {}
        }
    }
    let signer_infos = signer_infos.ok_or_else(|| not_cms("no SignerInfos".into()))?;
    if signer_infos.is_empty() {
        return Err("the CMS structure has no signer".into());
    }
    let mut first = None;
    for si in &signer_infos {
        let v = verify_signer_info(si, &content, &certs)?;
        first.get_or_insert(v);
    }
    let v = first.ok_or("the CMS structure has no signer")?;
    Ok((content, v))
}

fn verify_signer_info(
    si: &Tlv,
    content: &[u8],
    certs: &[(Certificate, Vec<u8>)],
) -> Result<Verified, String> {
    let f = si.expect(asn1::SEQUENCE, "SignerInfo")?;
    if f.len() < 5 {
        return Err("SignerInfo is incomplete".into());
    }
    let sid = &f[1];
    let cert = certs
        .iter()
        .find(|(c, _)| matches_sid(c, sid))
        .ok_or("the signer's certificate is not included in the signature")?;
    let who = signer::display_name(&cert.0.tbs_certificate.subject);

    let digest_alg = f[2].expect(asn1::SEQUENCE, "digestAlgorithm")?;
    if !digest_alg.first().is_some_and(|o| o.is_oid(&oids::SHA256)) {
        let name = digest_alg
            .first()
            .map(|o| o.oid_string())
            .unwrap_or_default();
        return Err(format!(
            "the signature of `{who}` uses digest algorithm {name}: only SHA-256 is supported"
        ));
    }
    let mut idx = 3;
    let mut signed_attrs = None;
    if f[idx].tag == 0xa0 {
        signed_attrs = Some(f[idx]);
        idx += 1;
    }
    let sig_alg = f
        .get(idx)
        .ok_or("SignerInfo without signature algorithm")?
        .expect(asn1::SEQUENCE, "signatureAlgorithm")?;
    let sig_alg = sig_alg.first().ok_or("empty signature algorithm")?;
    if ![
        oids::RSA_ENCRYPTION,
        oids::SHA256_WITH_RSA,
        oids::ECDSA_WITH_SHA256,
        oids::EC_PUBLIC_KEY,
    ]
    .iter()
    .any(|o| sig_alg.is_oid(o))
    {
        return Err(format!("the signature of `{who}` uses algorithm {}: only RSA PKCS#1 v1.5 and ECDSA with SHA-256 are supported", sig_alg.oid_string()));
    }
    let signature = f
        .get(idx + 1)
        .ok_or("SignerInfo without signature")?
        .octets()?;
    let content_digest = Sha256::digest(content).to_vec();

    let mut signing_time = None;
    let message: Vec<u8> = match signed_attrs {
        None => content.to_vec(),
        Some(sa) => {
            let mut found_digest = false;
            for attr in sa.children()? {
                let a = attr.expect(asn1::SEQUENCE, "Attribute")?;
                let (Some(oid), Some(values)) = (a.first(), a.get(1)) else {
                    continue;
                };
                let value = values
                    .children()?
                    .into_iter()
                    .next()
                    .ok_or("attribute without value")?;
                if oid.is_oid(&oids::MESSAGE_DIGEST) {
                    found_digest = true;
                    if *value.octets()? != *content_digest {
                        return Err(format!("the signed content was changed after `{who}` signed it (its SHA-256 digest does not match)"));
                    }
                } else if oid.is_oid(&oids::SIGNING_TIME) {
                    signing_time = Some(decode_time(&value)?);
                } else if oid.is_oid(&oids::SIGNING_CERTIFICATE_V2) {
                    check_signing_cert_v2(&value, &cert.1, &who)?;
                } else if oid.is_oid(&oids::SIGNING_CERTIFICATE) {
                    check_signing_cert_v1(&value, &cert.1, &who)?;
                }
            }
            if !found_digest {
                return Err("the signed attributes have no messageDigest".into());
            }
            let mut m = sa.raw.to_vec();
            m[0] = asn1::SET;
            m
        }
    };
    signer::verify_with_cert(&cert.0, &message, &signature, EcdsaFormat::Der)?;
    if let Some(t) = signing_time {
        signer::check_cert_time(&cert.0, t)?;
    }
    let others = certs.iter().map(|(_, d)| d.clone()).collect();
    Ok(signer::verified(
        &cert.0,
        cert.1.clone(),
        others,
        signing_time,
        true,
    ))
}

fn matches_sid(c: &Certificate, sid: &Tlv) -> bool {
    match sid.tag {
        asn1::SEQUENCE => {
            let Ok(kids) = sid.children() else {
                return false;
            };
            let (Some(issuer), Some(serial)) = (kids.first(), kids.get(1)) else {
                return false;
            };
            let Ok(ci) = c.tbs_certificate.issuer.to_der() else {
                return false;
            };
            issuer.raw == ci.as_slice() && serial.content == signer::serial_bytes(c)
        }
        0x80 => subject_key_id(c).is_some_and(|k| k == sid.content),
        _ => false,
    }
}

fn subject_key_id(c: &Certificate) -> Option<Vec<u8>> {
    let exts = c.tbs_certificate.extensions.as_ref()?;
    let e = exts.iter().find(|e| e.extn_id.to_string() == "2.5.29.14")?;
    let t = asn1::read_whole(e.extn_value.as_bytes()).ok()?;
    Some(t.octets().ok()?.into_owned())
}

/// signing-certificate-v2: the first ESSCertIDv2 must hash to the signer's certificate.
fn check_signing_cert_v2(value: &Tlv, cert_der: &[u8], who: &str) -> Result<(), String> {
    let certs = value.expect(asn1::SEQUENCE, "SigningCertificateV2")?;
    let ids = certs
        .first()
        .ok_or("empty SigningCertificateV2")?
        .expect(asn1::SEQUENCE, "certs")?;
    let id = ids
        .first()
        .ok_or("SigningCertificateV2 without certificates")?
        .expect(asn1::SEQUENCE, "ESSCertIDv2")?;
    let mut i = 0;
    let mut is_sha256 = true;
    if id.first().is_some_and(|t| t.tag == asn1::SEQUENCE) {
        let alg = id[0].children()?;
        is_sha256 = alg.first().is_some_and(|o| o.is_oid(&oids::SHA256));
        i = 1;
    }
    let hash = id.get(i).ok_or("ESSCertIDv2 without hash")?.octets()?;
    if is_sha256 && *hash != *signer::cert_sha256(cert_der) {
        return Err(format!("the signing-certificate attribute does not match the certificate of `{who}` included in the signature"));
    }
    Ok(())
}

/// signing-certificate (v1): SHA-1 of the signer's certificate.
fn check_signing_cert_v1(value: &Tlv, cert_der: &[u8], who: &str) -> Result<(), String> {
    let certs = value.expect(asn1::SEQUENCE, "SigningCertificate")?;
    let ids = certs
        .first()
        .ok_or("empty SigningCertificate")?
        .expect(asn1::SEQUENCE, "certs")?;
    let id = ids
        .first()
        .ok_or("SigningCertificate without certificates")?
        .expect(asn1::SEQUENCE, "ESSCertID")?;
    let hash = id.first().ok_or("ESSCertID without hash")?.octets()?;
    if *hash != *sha1::Sha1::digest(cert_der) {
        return Err(format!("the signing-certificate attribute does not match the certificate of `{who}` included in the signature"));
    }
    Ok(())
}
