//! XAdES-BES enveloped signatures (ETSI TS 101 903 v1.4.1, namespace v1.3.2), as
//! FatturaPA uses them.

use crate::pem::{b64_decode, b64_encode};
use crate::signer::{self, EcdsaFormat, Signer};
use crate::xml::{self, Document};
use crate::{SigningTime, Verified};
use der::Decode;
use sha2::{Digest, Sha256};
use x509_cert::Certificate;

pub(crate) const NS_DS: &str = "http://www.w3.org/2000/09/xmldsig#";
pub(crate) const NS_XADES: &str = "http://uri.etsi.org/01903/v1.3.2#";
const C14N: &str = "http://www.w3.org/TR/2001/REC-xml-c14n-20010315";
const ENVELOPED: &str = "http://www.w3.org/2000/09/xmldsig#enveloped-signature";
pub(crate) const RSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256";
pub(crate) const ECDSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#ecdsa-sha256";
const DIGEST_SHA256: &str = "http://www.w3.org/2001/04/xmlenc#sha256";
const DIGEST_SHA1: &str = "http://www.w3.org/2000/09/xmldsig#sha1";
const SIGNED_PROPERTIES_TYPE: &str = "http://uri.etsi.org/01903#SignedProperties";

const SIG_ID: &str = "quadra-signature";

/// Signs an XML document with an enveloped XAdES-BES signature and returns the signed
/// document.
///
/// The `ds:Signature` element is added as the last child of the root element; the rest
/// of the text is kept exactly as given. It has two references, both with SHA-256
/// digests and inclusive Canonical XML 1.0: the whole document (`URI=""`, with the
/// enveloped-signature transform) and the `xades:SignedProperties`, which hold the
/// `SigningTime` (`time`) and the `SigningCertificate` (SHA-256 digest of the
/// certificate, issuer and serial number). `KeyInfo` carries the signer's certificate.
/// This is the layout used for FatturaPA invoices (XAdES-BES, namespace
/// `http://uri.etsi.org/01903/v1.3.2#`).
///
/// The XML must be UTF-8 without a DTD or processing instructions (see the errors), and
/// must not be signed already. The output is the same for the same XML, key and time.
pub fn xades_enveloped(
    xml_text: &str,
    signer: &Signer,
    time: SigningTime,
) -> Result<String, String> {
    signer.check_valid_at(time)?;
    let doc = xml::parse(xml_text)?;
    let root = doc.root;
    if doc
        .descendants(root)
        .into_iter()
        .any(|e| doc.is(e, NS_DS, "Signature"))
    {
        return Err("the XML is already signed (it contains a ds:Signature)".into());
    }
    if doc.descendants(root).into_iter().any(|e| {
        ["Id", "ID", "id"]
            .iter()
            .any(|n| doc.attr(e, n).is_some_and(|v| v.starts_with(SIG_ID)))
    }) {
        return Err(format!(
            "the XML already uses identifiers starting with `{SIG_ID}`"
        ));
    }
    let doc_digest = b64_encode(&Sha256::digest(xml::c14n(&doc, root, None).as_bytes()));

    let leaf = signer.leaf();
    let cert_digest = b64_encode(&signer::cert_sha256(signer.certificate_der()));
    let signed_props = format!(
        concat!(
            "<xades:SignedProperties Id=\"{id}-signedprops\"><xades:SignedSignatureProperties>",
            "<xades:SigningTime>{time}</xades:SigningTime>",
            "<xades:SigningCertificate><xades:Cert><xades:CertDigest>",
            "<ds:DigestMethod Algorithm=\"{sha256}\"></ds:DigestMethod><ds:DigestValue>{cert_digest}</ds:DigestValue>",
            "</xades:CertDigest><xades:IssuerSerial>",
            "<ds:X509IssuerName>{issuer}</ds:X509IssuerName><ds:X509SerialNumber>{serial}</ds:X509SerialNumber>",
            "</xades:IssuerSerial></xades:Cert></xades:SigningCertificate>",
            "</xades:SignedSignatureProperties></xades:SignedProperties>"
        ),
        id = SIG_ID,
        time = time,
        sha256 = DIGEST_SHA256,
        cert_digest = cert_digest,
        issuer = xml::escape(&leaf.tbs_certificate.issuer.to_string()),
        serial = signer::serial_decimal(leaf),
    );
    let build = |sp_digest: &str, value: &str| {
        format!(
            concat!(
                "<ds:Signature xmlns:ds=\"{ds}\" Id=\"{id}\"><ds:SignedInfo>",
                "<ds:CanonicalizationMethod Algorithm=\"{c14n}\"></ds:CanonicalizationMethod>",
                "<ds:SignatureMethod Algorithm=\"{method}\"></ds:SignatureMethod>",
                "<ds:Reference Id=\"{id}-ref-document\" URI=\"\"><ds:Transforms><ds:Transform Algorithm=\"{env}\"></ds:Transform></ds:Transforms>",
                "<ds:DigestMethod Algorithm=\"{sha256}\"></ds:DigestMethod><ds:DigestValue>{doc_digest}</ds:DigestValue></ds:Reference>",
                "<ds:Reference Type=\"{sp_type}\" URI=\"#{id}-signedprops\">",
                "<ds:DigestMethod Algorithm=\"{sha256}\"></ds:DigestMethod><ds:DigestValue>{sp_digest}</ds:DigestValue></ds:Reference>",
                "</ds:SignedInfo><ds:SignatureValue Id=\"{id}-value\">{value}</ds:SignatureValue>",
                "<ds:KeyInfo><ds:X509Data><ds:X509Certificate>{cert}</ds:X509Certificate></ds:X509Data></ds:KeyInfo>",
                "<ds:Object><xades:QualifyingProperties xmlns:xades=\"{xades}\" Target=\"#{id}\">{signed_props}</xades:QualifyingProperties></ds:Object>",
                "</ds:Signature>"
            ),
            ds = NS_DS,
            xades = NS_XADES,
            id = SIG_ID,
            c14n = C14N,
            method = signer.xmldsig_method(),
            env = ENVELOPED,
            sha256 = DIGEST_SHA256,
            doc_digest = doc_digest,
            sp_type = SIGNED_PROPERTIES_TYPE,
            sp_digest = sp_digest,
            value = value,
            cert = b64_encode(signer.certificate_der()),
            signed_props = signed_props,
        )
    };
    let root_el = doc.elem(root);
    let splice = |sig: &str| -> String {
        let at = root_el.end_tag_start;
        if root_el.self_closing {
            let name = match &root_el.prefix {
                Some(p) => format!("{p}:{}", root_el.local),
                None => root_el.local.clone(),
            };
            format!("{}>{sig}</{name}>{}", &xml_text[..at], &xml_text[at + 2..])
        } else {
            format!("{}{sig}{}", &xml_text[..at], &xml_text[at..])
        }
    };

    // The digests and the signature are computed on the final document, so that the
    // namespaces in scope are exactly those a verifier sees.
    let step1 = splice(&build("", ""));
    let d1 = xml::parse(&step1)?;
    let sp = d1
        .by_id(&format!("{SIG_ID}-signedprops"))
        .ok_or("internal error: SignedProperties not found")?;
    let sp_digest = b64_encode(&Sha256::digest(xml::c14n(&d1, sp, None).as_bytes()));
    let step2 = splice(&build(&sp_digest, ""));
    let d2 = xml::parse(&step2)?;
    let sig_el = d2
        .by_id(SIG_ID)
        .ok_or("internal error: Signature not found")?;
    let si = d2
        .child(sig_el, NS_DS, "SignedInfo")
        .ok_or("internal error: SignedInfo not found")?;
    let value = signer.sign_xmldsig(xml::c14n(&d2, si, None).as_bytes())?;
    Ok(splice(&build(&sp_digest, &b64_encode(&value))))
}

/// Verifies an enveloped XAdES signature and describes the signer.
///
/// Checks the signature value over the canonical `SignedInfo` with the certificate in
/// `KeyInfo`, the digest of every reference (the document without the signature, and
/// the signed properties), and that the signed `SigningCertificate` (or
/// `SigningCertificateV2`) names that certificate. Supported: inclusive C14N 1.0,
/// SHA-256, RSA and ECDSA P-256. The certificate's trust is not checked: see
/// [`Verified`].
pub fn verify_xades(xml_text: &str) -> Result<Verified, String> {
    let doc = xml::parse(xml_text)?;
    let sigs: Vec<usize> = doc
        .descendants(doc.root)
        .into_iter()
        .filter(|&e| doc.is(e, NS_DS, "Signature"))
        .collect();
    let sig =
        match sigs.as_slice() {
            [] => return Err("the XML has no ds:Signature".into()),
            [s] => *s,
            _ => return Err(
                "the XML has more than one ds:Signature: only one enveloped signature is supported"
                    .into(),
            ),
        };
    let need = |parent: usize, ns: &str, name: &str| {
        doc.child(parent, ns, name)
            .ok_or_else(|| format!("the signature has no {name} element"))
    };
    let si = need(sig, NS_DS, "SignedInfo")?;
    let cm = need(si, NS_DS, "CanonicalizationMethod")?;
    let alg = doc.attr(cm, "Algorithm").unwrap_or("");
    if alg != C14N {
        return Err(format!("unsupported canonicalization `{alg}`: only inclusive Canonical XML 1.0 without comments is supported"));
    }
    let sm = need(si, NS_DS, "SignatureMethod")?;
    let method = doc.attr(sm, "Algorithm").unwrap_or("");
    if method != RSA_SHA256 && method != ECDSA_SHA256 {
        return Err(format!("unsupported signature method `{method}`: only RSA-SHA256 and ECDSA-SHA256 are supported"));
    }
    let value = b64_decode(&doc.text(need(sig, NS_DS, "SignatureValue")?))
        .map_err(|e| format!("the SignatureValue is not base64: {e}"))?;
    let key_info = need(sig, NS_DS, "KeyInfo")?;
    let x509 = need(key_info, NS_DS, "X509Data")?;
    let certs: Vec<Vec<u8>> = doc
        .child_elems(x509)
        .filter(|&c| doc.is(c, NS_DS, "X509Certificate"))
        .map(|c| b64_decode(&doc.text(c)))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("an X509Certificate is not base64: {e}"))?;
    let cert_der = certs
        .first()
        .ok_or("the signature carries no X509Certificate")?
        .clone();
    let cert = Certificate::from_der(&cert_der)
        .map_err(|e| format!("the signer's certificate cannot be read: {e}"))?;
    let who = signer::display_name(&cert.tbs_certificate.subject);

    // The signature over SignedInfo.
    let fmt = if method == ECDSA_SHA256 {
        EcdsaFormat::Raw
    } else {
        EcdsaFormat::Der
    };
    if method == ECDSA_SHA256 && value.len() != 64 {
        return Err(format!("the ECDSA SignatureValue of `{who}` is malformed"));
    }
    signer::verify_with_cert(&cert, xml::c14n(&doc, si, None).as_bytes(), &value, fmt)?;

    // Every reference.
    let mut signed_props = None;
    let mut covers_document = false;
    for r in doc
        .child_elems(si)
        .filter(|&c| doc.is(c, NS_DS, "Reference"))
    {
        let uri = doc.attr(r, "URI").ok_or("a Reference has no URI")?;
        let mut enveloped = false;
        if let Some(ts) = doc.child(r, NS_DS, "Transforms") {
            for t in doc.child_elems(ts) {
                match doc.attr(t, "Algorithm").unwrap_or("") {
                    ENVELOPED => enveloped = true,
                    C14N => {}
                    other => {
                        return Err(format!(
                            "unsupported transform `{other}` in the reference `{uri}`"
                        ))
                    }
                }
            }
        }
        let dm = need(r, NS_DS, "DigestMethod")?;
        let dalg = doc.attr(dm, "Algorithm").unwrap_or("");
        if dalg != DIGEST_SHA256 {
            return Err(format!(
                "unsupported digest `{dalg}` in the reference `{uri}`: only SHA-256 is supported"
            ));
        }
        let expected = b64_decode(&doc.text(need(r, NS_DS, "DigestValue")?))
            .map_err(|e| format!("a DigestValue is not base64: {e}"))?;
        let (target, what) = if uri.is_empty() {
            if !enveloped {
                return Err(
                    "the reference to the whole document lacks the enveloped-signature transform"
                        .into(),
                );
            }
            covers_document = true;
            (
                doc.root,
                "the XML document was changed after it was signed".to_string(),
            )
        } else if let Some(id) = uri.strip_prefix('#') {
            let t = doc
                .by_id(id)
                .ok_or_else(|| format!("the reference `{uri}` points to no element"))?;
            if doc.attr(r, "Type") == Some(SIGNED_PROPERTIES_TYPE)
                || doc.is(t, NS_XADES, "SignedProperties")
            {
                signed_props = Some(t);
            }
            (
                t,
                format!("the element `{uri}` was changed after it was signed"),
            )
        } else {
            return Err(format!(
                "unsupported reference `{uri}`: only references inside the document are supported"
            ));
        };
        let exclude = if enveloped { Some(sig) } else { None };
        let actual = Sha256::digest(xml::c14n(&doc, target, exclude).as_bytes());
        if actual.as_slice() != expected.as_slice() {
            return Err(what);
        }
    }
    if !covers_document {
        return Err(
            "the signature does not cover the document (no reference with URI=\"\")".into(),
        );
    }
    let sp = signed_props
        .ok_or("this is not a XAdES signature: no signed reference to xades:SignedProperties")?;

    // Signed properties: signing time and signing certificate.
    let ssp = doc
        .child(sp, NS_XADES, "SignedSignatureProperties")
        .ok_or("the signed properties have no SignedSignatureProperties")?;
    let signing_time = match doc.child(ssp, NS_XADES, "SigningTime") {
        Some(t) => {
            Some(parse_xml_datetime(doc.text(t).trim()).ok_or("the SigningTime cannot be read")?)
        }
        None => None,
    };
    let sc = doc
        .child(ssp, NS_XADES, "SigningCertificate")
        .or_else(|| doc.child(ssp, NS_XADES, "SigningCertificateV2"))
        .ok_or(
            "the signed properties do not identify the signing certificate (no SigningCertificate)",
        )?;
    check_signing_certificate(&doc, sc, &cert_der, &who)?;
    if let Some(t) = signing_time {
        signer::check_cert_time(&cert, t)?;
    }
    Ok(signer::verified(&cert, cert_der, certs, signing_time, true))
}

fn check_signing_certificate(
    doc: &Document,
    sc: usize,
    cert_der: &[u8],
    who: &str,
) -> Result<(), String> {
    for c in doc.child_elems(sc).filter(|&c| doc.is(c, NS_XADES, "Cert")) {
        let Some(cd) = doc.child(c, NS_XADES, "CertDigest") else {
            continue;
        };
        let (Some(dm), Some(dv)) = (
            doc.child(cd, NS_DS, "DigestMethod"),
            doc.child(cd, NS_DS, "DigestValue"),
        ) else {
            continue;
        };
        let expected = b64_decode(&doc.text(dv)).unwrap_or_default();
        let actual = match doc.attr(dm, "Algorithm") {
            Some(DIGEST_SHA256) => Sha256::digest(cert_der).to_vec(),
            Some(DIGEST_SHA1) => sha1::Sha1::digest(cert_der).to_vec(),
            _ => continue,
        };
        if actual == expected {
            return Ok(());
        }
    }
    Err(format!(
        "the signed SigningCertificate does not match the certificate of `{who}` in KeyInfo"
    ))
}

/// `YYYY-MM-DDTHH:MM:SS[.fff](Z|+hh:mm|-hh:mm)`; without a zone, UTC.
fn parse_xml_datetime(s: &str) -> Option<SigningTime> {
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<u16>().ok();
    let dt = der::DateTime::new(
        n(0..4)?,
        n(5..7)? as u8,
        n(8..10)? as u8,
        n(11..13)? as u8,
        n(14..16)? as u8,
        n(17..19)? as u8,
    )
    .ok()?;
    let mut secs = dt.unix_duration().as_secs() as i64;
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    match rest.chars().next() {
        None | Some('Z') => {}
        Some(sign @ ('+' | '-')) => {
            let hh: i64 = rest.get(1..3)?.parse().ok()?;
            let mm: i64 = rest.get(4..6)?.parse().ok()?;
            let off = hh * 3600 + mm * 60;
            secs += if sign == '+' { -off } else { off };
        }
        _ => return None,
    }
    Some(SigningTime::from_unix_seconds(u64::try_from(secs).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_datetimes() {
        let t = SigningTime::from_unix_seconds(1_780_315_200);
        assert_eq!(parse_xml_datetime("2026-06-01T12:00:00Z"), Some(t));
        assert_eq!(parse_xml_datetime("2026-06-01T14:00:00.250+02:00"), Some(t));
        assert_eq!(parse_xml_datetime("2026-06-01T12:00:00"), Some(t));
        assert_eq!(t.to_string(), "2026-06-01T12:00:00Z");
    }
}
