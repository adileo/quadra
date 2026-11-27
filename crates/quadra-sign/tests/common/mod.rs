//! Fixtures and helpers shared by the integration tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// 2026-06-01T12:00:00Z: inside the validity of the fixture certificates.
pub const T: u64 = 1_780_315_200;
/// 2031-01-01T00:00:00Z: after the fixture leaf certificates expired (2030-12-31).
pub const AFTER_EXPIRY: u64 = 1_924_992_000;
/// 2026-11-27T19:00:00Z: after the fixture OpenPGP keys were created (18:55Z) and before
/// the tests were written, so that gpg never sees a signature from the future.
pub const PGP_T: u64 = 1_795_806_000;

pub const CA_PEM: &str = include_str!("../fixtures/ca.pem");
pub const RSA_KEY: &str = include_str!("../fixtures/rsa.key");
pub const RSA_PKCS1_KEY: &str = include_str!("../fixtures/rsa-pkcs1.key");
pub const RSA_PEM: &str = include_str!("../fixtures/rsa.pem");
pub const EC_KEY: &str = include_str!("../fixtures/ec.key");
pub const EC_SEC1_KEY: &str = include_str!("../fixtures/ec-sec1.key");
pub const EC_PEM: &str = include_str!("../fixtures/ec.pem");
pub const RSA_P12: &[u8] = include_bytes!("../fixtures/rsa.p12");
pub const RSA_LEGACY_P12: &[u8] = include_bytes!("../fixtures/rsa-legacy.p12");
pub const RSA_3DES_P12: &[u8] = include_bytes!("../fixtures/rsa-3des.p12");
pub const EC_P12: &[u8] = include_bytes!("../fixtures/ec.p12");
pub const EC_PBMAC1_P12: &[u8] = include_bytes!("../fixtures/ec-pbmac1.p12");
pub const P12_PASSWORD: &str = "quadra-test";
pub const PGP_SIGNER_SEC: &str = include_str!("../fixtures/pgp-signer.sec.asc");
pub const PGP_SIGNER_PUB: &str = include_str!("../fixtures/pgp-signer.pub.asc");
pub const PGP_SIGNER_PASS: &str = "pgp-pass";
pub const PGP_BANK_SEC: &str = include_str!("../fixtures/pgp-bank.sec.asc");
pub const PGP_BANK_PUB: &str = include_str!("../fixtures/pgp-bank.pub.asc");

pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

pub fn rsa_signer() -> quadra_sign::Signer {
    quadra_sign::Signer::from_pem(RSA_KEY, &format!("{RSA_PEM}{CA_PEM}")).unwrap()
}

pub fn ec_signer() -> quadra_sign::Signer {
    quadra_sign::Signer::from_pem(EC_KEY, &format!("{EC_PEM}{CA_PEM}")).unwrap()
}

pub fn t() -> quadra_sign::SigningTime {
    quadra_sign::SigningTime::from_unix_seconds(T)
}

/// A fresh, empty directory for one test.
pub fn tmp(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("qs-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// An external tool, if installed.
pub fn tool(name: &str) -> Option<PathBuf> {
    for dir in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/usr/sbin",
    ] {
        let p = Path::new(dir).join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

#[macro_export]
macro_rules! require_tool {
    ($name:expr) => {
        match common::tool($name) {
            Some(p) => p,
            None => {
                eprintln!(
                    "note: `{}` is not installed: skipping this external check",
                    $name
                );
                return;
            }
        }
    };
}

pub fn run(cmd: &mut Command) -> Output {
    let out = cmd.output().expect("cannot run the external tool");
    let stdout = match std::str::from_utf8(&out.stdout) {
        Ok(s) if !s.contains('\0') => s.to_string(),
        _ => format!("({} bytes of binary output)\n", out.stdout.len()),
    };
    eprintln!(
        "$ {:?}\n{}{}",
        cmd,
        stdout,
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

pub fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// `python3` when it has lxml.
pub fn python_with_lxml() -> Option<PathBuf> {
    let mut candidates = ["python3"].iter().map(PathBuf::from).chain(tool("python3"));
    candidates.find(|p| {
        Command::new(p)
            .args(["-c", "import lxml.etree"])
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

// ------------------------------------------------------------------ PDFs

const CONTENT: &[u8] = b"BT /F1 24 Tf 72 720 Td (Fattura 2026/001 - ACME S.p.A.) Tj ET";

/// A one-page PDF 1.7 with a classic cross-reference table.
pub fn classic_pdf() -> Vec<u8> {
    let mut stream = format!("<< /Length {} >>\nstream\n", CONTENT.len()).into_bytes();
    stream.extend_from_slice(CONTENT);
    stream.extend_from_slice(b"\nendstream");
    let objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(),
        stream,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    let mut out = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R /ID [<0123456789ABCDEF0123456789ABCDEF> <0123456789ABCDEF0123456789ABCDEF>] >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// A one-page PDF 1.7 whose catalog, page tree and page live in a compressed object
/// stream, with a compressed cross-reference stream using the PNG Up predictor.
pub fn xref_stream_pdf() -> Vec<u8> {
    let in_stream: [&[u8]; 3] = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>",
    ];
    let mut body = Vec::new();
    let mut header = String::new();
    for (i, o) in in_stream.iter().enumerate() {
        header.push_str(&format!("{} {} ", i + 1, body.len()));
        body.extend_from_slice(o);
        body.push(b'\n');
    }
    let mut objstm = header.clone().into_bytes();
    objstm.extend_from_slice(&body);
    let objstm_z = miniz_oxide::deflate::compress_to_vec_zlib(&objstm, 6);

    let mut out = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut off = [0usize; 8];
    off[4] = out.len();
    out.extend_from_slice(format!("4 0 obj\n<< /Length {} >>\nstream\n", CONTENT.len()).as_bytes());
    out.extend_from_slice(CONTENT);
    out.extend_from_slice(b"\nendstream\nendobj\n");
    off[5] = out.len();
    out.extend_from_slice(
        b"5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n",
    );
    off[6] = out.len();
    out.extend_from_slice(
        format!(
            "6 0 obj\n<< /Type /ObjStm /N 3 /First {} /Filter /FlateDecode /Length {} >>\nstream\n",
            header.len(),
            objstm_z.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&objstm_z);
    out.extend_from_slice(b"\nendstream\nendobj\n");
    off[7] = out.len();
    // Rows: type (1 byte), field 2 (4 bytes), field 3 (2 bytes).
    let mut rows: Vec<[u8; 7]> = Vec::new();
    let row = |t: u8, f2: u32, f3: u16| {
        let mut r = [0u8; 7];
        r[0] = t;
        r[1..5].copy_from_slice(&f2.to_be_bytes());
        r[5..7].copy_from_slice(&f3.to_be_bytes());
        r
    };
    rows.push(row(0, 0, 65535));
    for i in 0..3 {
        rows.push(row(2, 6, i));
    }
    for o in &off[4..8] {
        rows.push(row(1, *o as u32, 0));
    }
    // PNG Up predictor.
    let mut predicted = Vec::new();
    let mut prev = [0u8; 7];
    for r in &rows {
        predicted.push(2u8);
        for i in 0..7 {
            predicted.push(r[i].wrapping_sub(prev[i]));
        }
        prev = *r;
    }
    let xz = miniz_oxide::deflate::compress_to_vec_zlib(&predicted, 6);
    out.extend_from_slice(
        format!(
            "7 0 obj\n<< /Type /XRef /Size 8 /W [1 4 2] /Root 1 0 R /ID [<00112233445566778899AABBCCDDEEFF> <00112233445566778899AABBCCDDEEFF>] /Filter /FlateDecode /DecodeParms << /Columns 7 /Predictor 12 >> /Length {} >>\nstream\n",
            xz.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&xz);
    out.extend_from_slice(
        format!("\nendstream\nendobj\nstartxref\n{}\n%%EOF\n", off[7]).as_bytes(),
    );
    out
}

/// The CMS signature and the signed bytes of the newest signature in a PDF.
pub fn last_pdf_signature(pdf: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let key = b"/ByteRange";
    let pos = pdf
        .windows(key.len())
        .rposition(|w| w == key)
        .expect("no /ByteRange");
    let rest = &pdf[pos + key.len()..];
    let open = rest.iter().position(|&c| c == b'[').unwrap();
    let close = rest.iter().position(|&c| c == b']').unwrap();
    let nums: Vec<usize> = std::str::from_utf8(&rest[open + 1..close])
        .unwrap()
        .split_whitespace()
        .map(|n| n.parse().unwrap())
        .collect();
    let (b, c, d) = (nums[1], nums[2], nums[3]);
    let hex = &pdf[b + 1..c - 1];
    let mut cms: Vec<u8> = hex
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect();
    let len = if cms[1] < 0x80 {
        2 + cms[1] as usize
    } else {
        let n = (cms[1] & 0x7f) as usize;
        2 + n
            + cms[2..2 + n]
                .iter()
                .fold(0usize, |a, &x| (a << 8) | x as usize)
    };
    cms.truncate(len);
    let mut signed = pdf[..b].to_vec();
    signed.extend_from_slice(&pdf[c..c + d]);
    (cms, signed)
}

// ------------------------------------------------------------------ XML

/// A small FatturaPA-like invoice.
pub const FATTURA: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:FatturaElettronica versione="FPR12" xmlns:p="http://ivaservizi.agenziaentrate.gov.it/docs/xsd/fatture/v1.2" xmlns:ds="http://www.w3.org/2000/09/xmldsig#" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://ivaservizi.agenziaentrate.gov.it/docs/xsd/fatture/v1.2 http://www.fatturapa.gov.it/export/fatturazione/sdi/fatturapa/v1.2/Schema_del_file_xml_FatturaPA_versione_1.2.xsd">
  <FatturaElettronicaHeader>
    <DatiTrasmissione>
      <IdTrasmittente>
        <IdPaese>IT</IdPaese>
        <IdCodice>01234567890</IdCodice>
      </IdTrasmittente>
      <ProgressivoInvio>00001</ProgressivoInvio>
      <FormatoTrasmissione>FPR12</FormatoTrasmissione>
      <CodiceDestinatario>0000000</CodiceDestinatario>
    </DatiTrasmissione>
    <CedentePrestatore>
      <DatiAnagrafici>
        <Anagrafica><Denominazione>ACME S.p.A.</Denominazione></Anagrafica>
        <RegimeFiscale>RF01</RegimeFiscale>
      </DatiAnagrafici>
    </CedentePrestatore>
    <CessionarioCommittente>
      <DatiAnagrafici>
        <Anagrafica><Denominazione>Rossi &amp; Figli "Società" &lt;s.n.c.&gt;</Denominazione></Anagrafica>
      </DatiAnagrafici>
    </CessionarioCommittente>
  </FatturaElettronicaHeader>
  <FatturaElettronicaBody>
    <DatiGenerali>
      <DatiGeneraliDocumento>
        <TipoDocumento>TD01</TipoDocumento>
        <Divisa>EUR</Divisa>
        <Data>2026-06-01</Data>
        <Numero>2026/001</Numero>
        <ImportoTotaleDocumento>122.00</ImportoTotaleDocumento>
        <Causale>Servizi di consulenza &#x2014; giugno</Causale>
      </DatiGeneraliDocumento>
    </DatiGenerali>
    <DatiBeniServizi>
      <DettaglioLinee>
        <NumeroLinea>1</NumeroLinea>
        <Descrizione>Consulenza</Descrizione>
        <PrezzoTotale>100.00</PrezzoTotale>
        <AliquotaIVA>22.00</AliquotaIVA>
      </DettaglioLinee>
      <Empty/>
    </DatiBeniServizi>
  </FatturaElettronicaBody>
</p:FatturaElettronica>
"#;

/// The independent XAdES check: canonicalizes with lxml (libxml2), checks every
/// reference digest, and writes SignedInfo, the signature value (DER for ECDSA) and
/// the certificate for `openssl dgst -verify`.
pub const XADES_CHECK_PY: &str = r#"
import sys, base64, hashlib
from lxml import etree
DS = "http://www.w3.org/2000/09/xmldsig#"
src, out = sys.argv[1], sys.argv[2]
doc = etree.parse(src)
root = doc.getroot()
sig = root.find("{%s}Signature" % DS)
si = sig.find("{%s}SignedInfo" % DS)
open(out + "/signedinfo.c14n", "wb").write(etree.tostring(si, method="c14n", exclusive=False, with_comments=False))
sv = base64.b64decode(sig.find("{%s}SignatureValue" % DS).text)
method = si.find("{%s}SignatureMethod" % DS).get("Algorithm")
if method.endswith("ecdsa-sha256"):
    def der_int(b):
        b = b.lstrip(b"\0") or b"\0"
        if b[0] & 0x80:
            b = b"\0" + b
        return b"\x02" + bytes([len(b)]) + b
    body = der_int(sv[:32]) + der_int(sv[32:])
    sv = b"\x30" + bytes([len(body)]) + body
open(out + "/signature.bin", "wb").write(sv)
open(out + "/cert.der", "wb").write(base64.b64decode(sig.find(".//{%s}X509Certificate" % DS).text))
ok = True
for ref in si.findall("{%s}Reference" % DS):
    uri = ref.get("URI")
    expected = base64.b64decode(ref.find("{%s}DigestValue" % DS).text)
    if uri == "":
        copy = etree.fromstring(etree.tostring(root))
        s = copy.find("{%s}Signature" % DS)
        assert s.tail is None
        copy.remove(s)
        data = etree.tostring(copy, method="c14n", exclusive=False, with_comments=False)
    else:
        el = doc.xpath("//*[@Id=$id]", id=uri[1:])[0]
        data = etree.tostring(el, method="c14n", exclusive=False, with_comments=False)
    if hashlib.sha256(data).digest() != expected:
        print("DIGEST MISMATCH for reference URI=%r" % uri)
        ok = False
    else:
        print("digest OK for reference URI=%r" % uri)
sys.exit(0 if ok else 1)
"#;
