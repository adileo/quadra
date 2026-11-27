//! PAdES baseline B-B (ETSI EN 319 142-1): a CAdES detached signature in an
//! incremental update of the PDF.

use crate::cades;
use crate::pdf::{self, Dict, Doc, Obj};
use crate::signer::{self, Signer};
use crate::{PadesInfo, SigningTime, Verified};
use sha2::{Digest, Sha256};

/// Width of each number in the `/ByteRange` placeholder.
const BR_DIGITS: usize = 10;

/// Signs a PDF (PAdES B-B) and returns the signed PDF.
///
/// The original bytes are kept unchanged; an incremental update is appended with an
/// invisible signature field on the first page and a signature dictionary
/// (`/Filter /Adobe.PPKLite /SubFilter /ETSI.CAdES.detached`) whose `/Contents` holds a
/// detached CMS signature over the whole file except `/Contents` itself. The signing
/// time goes into `/M` (PAdES puts no signingTime inside the CMS). Files with classic
/// cross-reference tables and with cross-reference streams are both supported; the
/// update uses the same kind as the original. Encrypted PDFs are refused.
///
/// The output is the same for the same PDF, key, time and `info`.
pub fn pades(
    pdf_bytes: &[u8],
    signer: &Signer,
    time: SigningTime,
    info: &PadesInfo,
) -> Result<Vec<u8>, String> {
    signer.check_valid_at(time)?;
    let doc = Doc::load(pdf_bytes).map_err(|e| format!("the PDF cannot be signed: {e}"))?;
    let fail = |e: String| format!("the PDF cannot be signed: {e}");

    let root_ref = match doc.trailer.get("Root") {
        Some(Obj::Ref(n, _)) => *n,
        _ => {
            return Err(fail(
                "the trailer has no /Root (document catalog) reference".into(),
            ))
        }
    };
    let mut catalog = match doc.get(root_ref).map_err(fail)? {
        Obj::Dict(d) => d,
        _ => {
            return Err(fail(format!(
                "the document catalog (object {root_ref}) is not a dictionary"
            )))
        }
    };
    let (page_ref, mut page) = first_page(&doc, &catalog).map_err(fail)?;

    let mut next = doc.size();
    let sig_num = next;
    let field_num = next + 1;
    next += 2;
    // Objects of the update: number -> body.
    let mut updated: Vec<(u32, Vec<u8>)> = Vec::new();
    let write = |d: &Obj| {
        let mut v = Vec::new();
        pdf::write_obj(d, &mut v);
        v
    };

    // The signature field goes into /AcroForm /Fields.
    let field_ref = Obj::Ref(field_num, 0);
    let mut field_names = Vec::new();
    let acroform_obj = catalog.get("AcroForm").cloned();
    let (mut acroform, acroform_num) = match &acroform_obj {
        None | Some(Obj::Null) => (Dict::default(), None),
        Some(Obj::Ref(n, _)) => match doc.get(*n).map_err(fail)? {
            Obj::Dict(d) => (d, Some(*n)),
            Obj::Null => (Dict::default(), None),
            _ => return Err(fail(format!("/AcroForm (object {n}) is not a dictionary"))),
        },
        Some(Obj::Dict(d)) => (d.clone(), None),
        Some(_) => return Err(fail("/AcroForm is not a dictionary".into())),
    };
    match acroform.get("Fields").cloned() {
        Some(Obj::Ref(n, _)) => {
            let mut fields = match doc.get(n).map_err(fail)? {
                Obj::Array(a) => a,
                _ => {
                    return Err(fail(format!(
                        "/AcroForm /Fields (object {n}) is not an array"
                    )))
                }
            };
            collect_names(&doc, &fields, &mut field_names);
            fields.push(field_ref.clone());
            updated.push((n, write(&Obj::Array(fields))));
        }
        Some(Obj::Array(mut fields)) => {
            collect_names(&doc, &fields, &mut field_names);
            fields.push(field_ref.clone());
            acroform.set("Fields", Obj::Array(fields));
        }
        _ => acroform.set("Fields", Obj::Array(vec![field_ref.clone()])),
    }
    acroform.set("SigFlags", Obj::Int(3));
    match acroform_num {
        Some(n) => updated.push((n, write(&Obj::Dict(acroform)))),
        None => {
            catalog.set("AcroForm", Obj::Dict(acroform));
        }
    }
    updated.push((root_ref, write(&Obj::Dict(catalog))));

    // The widget goes into the first page's /Annots.
    match page.get("Annots").cloned() {
        Some(Obj::Ref(n, _)) => {
            let mut annots = match doc.get(n).map_err(fail)? {
                Obj::Array(a) => a,
                _ => {
                    return Err(fail(format!(
                        "the /Annots of the first page (object {n}) is not an array"
                    )))
                }
            };
            annots.push(field_ref.clone());
            updated.push((n, write(&Obj::Array(annots))));
        }
        Some(Obj::Array(mut annots)) => {
            annots.push(field_ref.clone());
            page.set("Annots", Obj::Array(annots));
            updated.push((page_ref, write(&Obj::Dict(page))));
        }
        _ => {
            page.set("Annots", Obj::Array(vec![field_ref.clone()]));
            updated.push((page_ref, write(&Obj::Dict(page))));
        }
    }

    let mut k = 1;
    while field_names
        .iter()
        .any(|n| *n == format!("Signature{k}").into_bytes())
    {
        k += 1;
    }
    let mut field = Dict::default();
    for (key, v) in [
        ("Type", Obj::Name(b"Annot".to_vec())),
        ("Subtype", Obj::Name(b"Widget".to_vec())),
        ("FT", Obj::Name(b"Sig".to_vec())),
        ("T", Obj::Str(format!("Signature{k}").into_bytes())),
        ("V", Obj::Ref(sig_num, 0)),
        ("F", Obj::Int(132)), // Print + Locked
        ("Rect", Obj::Array(vec![Obj::Int(0); 4])),
        ("P", Obj::Ref(page_ref, doc.generation(page_ref))),
    ] {
        field.set(key, v);
    }
    updated.push((field_num, write(&Obj::Dict(field))));

    // The signature dictionary, with placeholders for /ByteRange and /Contents.
    let dummy = cades::signed_data(signer, &[0u8; 32], None, None)?;
    let reserve = (dummy.len() + 2048).div_ceil(1024) * 1024;
    let br_placeholder = format!("[0 {z} {z} {z}]", z = "0".repeat(BR_DIGITS));
    let mut sig = Vec::new();
    sig.extend_from_slice(
        b"<</Type/Sig/Filter/Adobe.PPKLite/SubFilter/ETSI.CAdES.detached/ByteRange ",
    );
    let br_rel = sig.len();
    sig.extend_from_slice(br_placeholder.as_bytes());
    sig.extend_from_slice(b"/Contents ");
    let contents_rel = sig.len();
    sig.push(b'<');
    sig.extend(std::iter::repeat_n(b'0', reserve * 2));
    sig.push(b'>');
    sig.extend_from_slice(b"/M");
    pdf::write_string(pdf_date(time)?.as_bytes(), &mut sig);
    for (key, value) in [
        ("Name", &info.name),
        ("Reason", &info.reason),
        ("Location", &info.location),
    ] {
        if let Some(v) = value {
            sig.extend_from_slice(format!("/{key}").as_bytes());
            pdf::write_obj(&pdf::text_string(v), &mut sig);
        }
    }
    sig.extend_from_slice(b">>");
    updated.push((sig_num, sig));

    // Assemble the update.
    updated.sort_by_key(|(n, _)| *n);
    let mut out = pdf_bytes.to_vec();
    if !out.ends_with(b"\n") && !out.ends_with(b"\r") {
        out.push(b'\n');
    }
    let mut offsets = Vec::new();
    let mut br_pos = 0;
    let mut contents_pos = 0;
    for (n, body) in &updated {
        let gen = if *n >= sig_num { 0 } else { doc.generation(*n) };
        offsets.push((*n, out.len(), gen));
        let header = format!("{n} {gen} obj\n");
        out.extend_from_slice(header.as_bytes());
        if *n == sig_num {
            br_pos = out.len() + br_rel;
            contents_pos = out.len() + contents_rel;
        }
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let mut trailer = Dict::default();
    for key in ["Root", "Info", "ID"] {
        if let Some(v) = doc.trailer.get(key) {
            trailer.set(key, v.clone());
        }
    }
    if doc.uses_xref_stream {
        let xref_num = next;
        let xref_off = out.len();
        offsets.push((xref_num, xref_off, 0));
        let mut rows = Vec::new();
        for (_, off, gen) in &offsets {
            rows.push(1u8);
            rows.extend_from_slice(
                &u32::try_from(*off)
                    .map_err(|_| "the PDF is larger than 4 GB".to_string())?
                    .to_be_bytes(),
            );
            rows.extend_from_slice(&gen.to_be_bytes());
        }
        let mut d = Dict::default();
        d.set("Type", Obj::Name(b"XRef".to_vec()));
        d.set("Size", Obj::Int(xref_num as i64 + 1));
        d.set("W", Obj::Array(vec![Obj::Int(1), Obj::Int(4), Obj::Int(2)]));
        d.set("Index", Obj::Array(index_runs(&offsets)));
        for (k, v) in trailer.0 {
            d.0.push((k, v));
        }
        d.set("Prev", Obj::Int(doc.last_xref as i64));
        d.set("Length", Obj::Int(rows.len() as i64));
        out.extend_from_slice(format!("{xref_num} 0 obj\n").as_bytes());
        pdf::write_dict(&d, &mut out);
        out.extend_from_slice(b"\nstream\n");
        out.extend_from_slice(&rows);
        out.extend_from_slice(b"\nendstream\nendobj\n");
        out.extend_from_slice(format!("startxref\n{xref_off}\n%%EOF\n").as_bytes());
    } else {
        let xref_off = out.len();
        out.extend_from_slice(b"xref\n");
        let mut i = 0;
        while i < offsets.len() {
            let mut j = i;
            while j + 1 < offsets.len() && offsets[j + 1].0 == offsets[j].0 + 1 {
                j += 1;
            }
            out.extend_from_slice(format!("{} {}\n", offsets[i].0, j - i + 1).as_bytes());
            for (_, off, gen) in &offsets[i..=j] {
                out.extend_from_slice(format!("{off:010} {gen:05} n\r\n").as_bytes());
            }
            i = j + 1;
        }
        trailer.set("Size", Obj::Int(next as i64));
        trailer.set("Prev", Obj::Int(doc.last_xref as i64));
        out.extend_from_slice(b"trailer\n");
        // /Size first, as is customary.
        let mut t = Dict::default();
        t.set("Size", Obj::Int(next as i64));
        for (k, v) in trailer.0 {
            if k != b"Size" {
                t.0.push((k, v));
            }
        }
        pdf::write_dict(&t, &mut out);
        out.extend_from_slice(format!("\nstartxref\n{xref_off}\n%%EOF\n").as_bytes());
    }

    // Fill /ByteRange, then hash and sign.
    let hole_end = contents_pos + 2 + reserve * 2;
    let br = format!("[0 {contents_pos} {hole_end} {}]", out.len() - hole_end);
    if br.len() > br_placeholder.len() {
        return Err("the PDF is too large for the /ByteRange placeholder".into());
    }
    let br = format!("{br:<width$}", width = br_placeholder.len());
    out[br_pos..br_pos + br.len()].copy_from_slice(br.as_bytes());
    let mut h = Sha256::new();
    h.update(&out[..contents_pos]);
    h.update(&out[hole_end..]);
    let digest = h.finalize();
    let cms = cades::signed_data(signer, &digest, None, None)?;
    if cms.len() > reserve {
        return Err(format!(
            "the signature ({} bytes) does not fit the space reserved for it ({reserve} bytes)",
            cms.len()
        ));
    }
    let hex: Vec<u8> = cms
        .iter()
        .flat_map(|b| format!("{b:02X}").into_bytes())
        .collect();
    out[contents_pos + 1..contents_pos + 1 + hex.len()].copy_from_slice(&hex);
    Ok(out)
}

/// `[start count start count ...]` for sorted object numbers.
fn index_runs(offsets: &[(u32, usize, u16)]) -> Vec<Obj> {
    let mut v = Vec::new();
    let mut i = 0;
    while i < offsets.len() {
        let mut j = i;
        while j + 1 < offsets.len() && offsets[j + 1].0 == offsets[j].0 + 1 {
            j += 1;
        }
        v.push(Obj::Int(offsets[i].0 as i64));
        v.push(Obj::Int((j - i + 1) as i64));
        i = j + 1;
    }
    v
}

fn collect_names(doc: &Doc, fields: &[Obj], out: &mut Vec<Vec<u8>>) {
    for f in fields {
        if let Ok(Obj::Dict(d)) = doc.resolve(f) {
            if let Some(Obj::Str(t)) = d.get("T") {
                out.push(t.clone());
            }
        }
    }
}

/// The first page: its object number and dictionary.
fn first_page(doc: &Doc, catalog: &Dict) -> Result<(u32, Dict), String> {
    let mut node_ref = match catalog.get("Pages") {
        Some(Obj::Ref(n, _)) => *n,
        _ => return Err("the document catalog has no /Pages reference".into()),
    };
    for _ in 0..64 {
        let node = match doc.get(node_ref)? {
            Obj::Dict(d) => d,
            _ => return Err(format!("page tree node {node_ref} is not a dictionary")),
        };
        if node.name_is("Type", "Page") || node.get("Kids").is_none() {
            return Ok((node_ref, node));
        }
        let kids = match doc.resolve(node.get("Kids").unwrap_or(&Obj::Null))? {
            Obj::Array(a) => a,
            _ => {
                return Err(format!(
                    "the /Kids of page tree node {node_ref} is not an array"
                ))
            }
        };
        node_ref = match kids.first() {
            Some(Obj::Ref(n, _)) => *n,
            _ => return Err("the PDF has no pages".into()),
        };
    }
    Err("the page tree is too deep".into())
}

/// `D:YYYYMMDDHHmmSS+00'00'`
fn pdf_date(t: SigningTime) -> Result<String, String> {
    let d = t.datetime()?;
    Ok(format!(
        "D:{:04}{:02}{:02}{:02}{:02}{:02}+00'00'",
        d.year(),
        d.month(),
        d.day(),
        d.hour(),
        d.minutes(),
        d.seconds()
    ))
}

/// Reads a PDF date (`D:YYYYMMDDHHmmSSOHH'mm'`), with any of the optional parts.
fn parse_pdf_date(s: &[u8]) -> Option<SigningTime> {
    let s = std::str::from_utf8(s).ok()?;
    let s = s.strip_prefix("D:").unwrap_or(s);
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    let field = |from: usize, len: usize, default: u32| -> Option<u32> {
        match digits.get(from..from + len) {
            Some(x) => x.parse().ok(),
            None if digits.len() <= from => Some(default),
            None => None,
        }
    };
    let year = field(0, 4, 0)? as u16;
    let dt = der::DateTime::new(
        year,
        field(4, 2, 1)? as u8,
        field(6, 2, 1)? as u8,
        field(8, 2, 0)? as u8,
        field(10, 2, 0)? as u8,
        field(12, 2, 0)? as u8,
    )
    .ok()?;
    let mut secs = dt.unix_duration().as_secs() as i64;
    let rest = &s[digits.len()..];
    if let Some(sign @ ('+' | '-')) = rest.chars().next() {
        let tz: String = rest[1..].chars().filter(|c| c.is_ascii_digit()).collect();
        let hh: i64 = tz.get(0..2).and_then(|x| x.parse().ok()).unwrap_or(0);
        let mm: i64 = tz.get(2..4).and_then(|x| x.parse().ok()).unwrap_or(0);
        let off = hh * 3600 + mm * 60;
        secs += if sign == '+' { -off } else { off };
    }
    Some(SigningTime::from_unix_seconds(secs.max(0) as u64))
}

/// Verifies every signature in a PDF and describes the newest one.
///
/// For each signature field: the `/ByteRange` must cover the file except exactly the
/// `/Contents` string, and the CMS signature in `/Contents` must match those bytes
/// (`ETSI.CAdES.detached` or `adbe.pkcs7.detached`, SHA-256). Content appended after
/// the newest signature is reported through [`Verified::covers_whole_file`]. The
/// certificate's trust is not checked: see [`Verified`].
pub fn verify_pades(pdf_bytes: &[u8]) -> Result<Verified, String> {
    let doc = Doc::load(pdf_bytes).map_err(|e| format!("the PDF cannot be read: {e}"))?;
    let root = doc.resolve(doc.trailer.get("Root").unwrap_or(&Obj::Null))?;
    let catalog = root.as_dict().ok_or("the PDF has no document catalog")?;
    let acroform = doc.resolve(catalog.get("AcroForm").unwrap_or(&Obj::Null))?;
    let fields = match acroform.as_dict().and_then(|a| a.get("Fields")) {
        Some(f) => doc.resolve(f)?,
        None => return Err("the PDF has no signature".into()),
    };
    let mut sigs = Vec::new();
    let Obj::Array(fields) = fields else {
        return Err("the PDF's /AcroForm /Fields is not an array".into());
    };
    collect_sigs(&doc, &fields, false, 0, &mut sigs)?;
    if sigs.is_empty() {
        return Err("the PDF has no signature".into());
    }
    let mut best: Option<(usize, Verified)> = None;
    for (name, sig) in sigs {
        let v = verify_one(pdf_bytes, &sig).map_err(|e| format!("signature `{name}`: {e}"))?;
        let Some(v) = v else { continue };
        let end = v.0;
        if best.as_ref().is_none_or(|(e, _)| end > *e) {
            best = Some(v);
        }
    }
    best.map(|(_, v)| v)
        .ok_or_else(|| "the PDF has no signature".to_string())
}

fn collect_sigs(
    doc: &Doc,
    fields: &[Obj],
    parent_is_sig: bool,
    depth: usize,
    out: &mut Vec<(String, Dict)>,
) -> Result<(), String> {
    if depth > 32 {
        return Ok(());
    }
    for f in fields {
        let Obj::Dict(d) = doc.resolve(f)? else {
            continue;
        };
        let is_sig = match d.get("FT") {
            Some(Obj::Name(n)) => n == b"Sig",
            _ => parent_is_sig,
        };
        if let Some(Obj::Array(kids)) = d.get("Kids").map(|k| doc.resolve(k)).transpose()? {
            collect_sigs(doc, &kids, is_sig, depth + 1, out)?;
        }
        if is_sig {
            if let Some(v) = d.get("V") {
                if let Obj::Dict(sig) = doc.resolve(v)? {
                    let name = match d.get("T") {
                        Some(Obj::Str(t)) => String::from_utf8_lossy(t).into_owned(),
                        _ => "(unnamed)".into(),
                    };
                    out.push((name, sig));
                }
            }
        }
    }
    Ok(())
}

/// Verifies one signature dictionary. Returns the end of the signed range with the
/// result, or `None` for a document timestamp (not a signature by a certificate holder).
fn verify_one(data: &[u8], sig: &Dict) -> Result<Option<(usize, Verified)>, String> {
    if sig.name_is("Type", "DocTimeStamp") || sig.name_is("SubFilter", "ETSI.RFC3161") {
        return Ok(None);
    }
    if !(sig.name_is("SubFilter", "ETSI.CAdES.detached")
        || sig.name_is("SubFilter", "adbe.pkcs7.detached"))
    {
        let sf = match sig.get("SubFilter") {
            Some(Obj::Name(n)) => String::from_utf8_lossy(n).into_owned(),
            _ => "(none)".into(),
        };
        return Err(format!("unsupported signature format /SubFilter /{sf}"));
    }
    let br: Vec<i64> = match sig.get("ByteRange") {
        Some(Obj::Array(a)) => a
            .iter()
            .map(|x| if let Obj::Int(i) = x { *i } else { -1 })
            .collect(),
        _ => return Err("the signature has no /ByteRange".into()),
    };
    if br.len() != 4 || br.iter().any(|&x| x < 0) {
        return Err("the /ByteRange is not four non-negative numbers".into());
    }
    let (a, b, c, d) = (
        br[0] as usize,
        br[1] as usize,
        br[2] as usize,
        br[3] as usize,
    );
    if a != 0 || b >= c || c.checked_add(d).is_none_or(|e| e > data.len()) {
        return Err("the /ByteRange does not fit the file".into());
    }
    let contents = match sig.get("Contents") {
        Some(Obj::Str(s)) => s,
        _ => return Err("the signature has no /Contents".into()),
    };
    // The gap must be exactly the /Contents hex string.
    let hole = &data[b..c];
    let not_contents =
        || "the bytes left out by /ByteRange are not exactly the /Contents string".to_string();
    if hole.len() < 2 || hole[0] != b'<' || hole[hole.len() - 1] != b'>' {
        return Err(not_contents());
    }
    let inner = &hole[1..hole.len() - 1];
    if !inner.iter().all(|x| x.is_ascii_hexdigit()) {
        return Err(not_contents());
    }
    let hole_bytes: Vec<u8> = inner
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap_or("0"), 16).unwrap_or(0))
        .collect();
    if hole_bytes != *contents {
        return Err(
            "the bytes left out by /ByteRange are not the /Contents of this signature".into(),
        );
    }
    let (first, _) = crate::asn1::read_one(contents)
        .map_err(|e| format!("the CMS signature cannot be read: {e}"))?;
    let mut signed = Vec::with_capacity(b + d);
    signed.extend_from_slice(&data[..b]);
    signed.extend_from_slice(&data[c..c + d]);
    let (_, mut v) = cades::verify_signed_data(first.raw, Some(&signed))?;
    if v.signing_time.is_none() {
        if let Some(Obj::Str(m)) = sig.get("M") {
            v.signing_time = parse_pdf_date(m);
        }
        if let Some(t) = v.signing_time {
            let cert =
                x509_cert::Certificate::from_der(&v.certificates[0]).map_err(|e| e.to_string())?;
            signer::check_cert_time(&cert, t)?;
        }
    }
    v.covers_whole_file = c + d == data.len();
    Ok(Some((c + d, v)))
}

use der::Decode as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_dates() {
        let t = SigningTime::from_unix_seconds(1_780_315_200); // 2026-06-01T12:00:00Z
        assert_eq!(pdf_date(t).unwrap(), "D:20260601120000+00'00'");
        assert_eq!(parse_pdf_date(b"D:20260601120000+00'00'"), Some(t));
        assert_eq!(parse_pdf_date(b"D:20260601140000+02'00'"), Some(t));
        assert_eq!(parse_pdf_date(b"D:20260601120000Z"), Some(t));
    }
}
