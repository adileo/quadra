//! End-to-end tests: every format is signed, verified by this crate and, when the
//! tool is installed, by an independent implementation (OpenSSL, poppler's pdfsig,
//! GnuPG, libxml2 through xmllint and lxml). Missing tools skip their check with a note.

mod common;

use common::*;
use quadra_sign::*;
use std::fs;
use std::process::Command;

// ================================================================== keys

#[test]
fn pem_keys_in_every_supported_form() {
    let chain = format!("{RSA_PEM}{CA_PEM}");
    for key in [RSA_KEY, RSA_PKCS1_KEY] {
        let s = Signer::from_pem(key, &chain).unwrap();
        assert_eq!(s.subject(), "ACME S.p.A.");
        assert_eq!(s.subject_dn(), "CN=ACME S.p.A.,O=ACME S.p.A.,C=IT");
        assert_eq!(s.issuer_dn(), "CN=Quadra Test CA,O=Quadra Test,C=IT");
        assert_eq!(s.not_after().date(), "2030-12-31");
        assert_eq!(s.not_after().to_string(), "2030-12-31T23:59:59Z");
        assert_eq!(s.chain_der().len(), 2);
    }
    let chain = format!("{EC_PEM}{CA_PEM}");
    for key in [EC_KEY, EC_SEC1_KEY] {
        let s = Signer::from_pem(key, &chain).unwrap();
        assert_eq!(s.subject(), "Beta S.r.l.");
    }
    // The chain in the wrong order is put right.
    let s = Signer::from_pem(RSA_KEY, &format!("{CA_PEM}{RSA_PEM}")).unwrap();
    assert_eq!(s.subject(), "ACME S.p.A.");
    assert_eq!(s.chain_der().len(), 2);
}

#[test]
fn pkcs12_current_and_legacy_formats() {
    for (name, p12, subject) in [
        (
            "AES-256/PBKDF2/HMAC-SHA256 (OpenSSL 3+ default)",
            RSA_P12,
            "ACME S.p.A.",
        ),
        (
            "legacy RC2-40 + 3DES, SHA-1 MAC",
            RSA_LEGACY_P12,
            "ACME S.p.A.",
        ),
        ("3DES, SHA-1 MAC", RSA_3DES_P12, "ACME S.p.A."),
        ("EC, AES-256", EC_P12, "Beta S.r.l."),
        ("EC, PBMAC1", EC_PBMAC1_P12, "Beta S.r.l."),
    ] {
        let s = Signer::from_pkcs12(p12, P12_PASSWORD).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(s.subject(), subject, "{name}");
        assert_eq!(
            s.chain_der().len(),
            2,
            "{name}: the CA certificate is in the chain"
        );
    }
}

#[test]
fn key_errors_say_what_is_wrong() {
    for p12 in [RSA_P12, RSA_LEGACY_P12, EC_PBMAC1_P12] {
        let e = Signer::from_pkcs12(p12, "wrong").unwrap_err();
        assert!(e.contains("wrong password for the PKCS#12 file"), "{e}");
    }
    let e = Signer::from_pkcs12(b"not a p12", "x").unwrap_err();
    assert!(e.contains("not a PKCS#12 file"), "{e}");
    let e = Signer::from_pem(EC_KEY, RSA_PEM).unwrap_err();
    assert!(
        e.contains("does not match the certificate `ACME S.p.A.`"),
        "{e}"
    );
    let e = Signer::from_pem(
        "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----\n",
        RSA_PEM,
    )
    .unwrap_err();
    assert!(e.contains("encrypted"), "{e}");
    let e = Signer::from_pem(RSA_KEY, "").unwrap_err();
    assert!(e.contains("no certificate found"), "{e}");
    let e = Signer::from_pem("", RSA_PEM).unwrap_err();
    assert!(e.contains("no private key found"), "{e}");
}

#[test]
fn expired_certificate_is_refused_everywhere() {
    let s = rsa_signer();
    let late = SigningTime::from_unix_seconds(AFTER_EXPIRY);
    let expect = "the certificate of `ACME S.p.A.` expired on 2030-12-31";
    let e = cades_p7m(b"x", &s, late).unwrap_err();
    assert!(e.starts_with(expect), "{e}");
    let e = pades(&classic_pdf(), &s, late, &PadesInfo::default()).unwrap_err();
    assert!(e.starts_with(expect), "{e}");
    let e = xades_enveloped(FATTURA, &s, late).unwrap_err();
    assert!(e.starts_with(expect), "{e}");
    let early = SigningTime::from_unix_seconds(1_700_000_000); // 2023
    let e = cades_p7m(b"x", &s, early).unwrap_err();
    assert!(e.contains("is valid only from 2025-01-01"), "{e}");
}

// ================================================================== CAdES

#[test]
fn cades_p7m_round_trip_determinism_and_tampering() {
    let content = FATTURA.as_bytes();
    for signer in [rsa_signer(), ec_signer()] {
        let p7m = cades_p7m(content, &signer, t()).unwrap();
        assert_eq!(
            p7m,
            cades_p7m(content, &signer, t()).unwrap(),
            "same input, key and time: same bytes"
        );
        let (back, v) = verify_cades(&p7m).unwrap();
        assert_eq!(back, content);
        assert_eq!(v.subject, signer.subject());
        assert_eq!(v.signing_time, Some(t()));
        assert_eq!(v.certificates.len(), 2);
        // Base64 delivery is accepted too.
        let b64 = base64_encode(&p7m);
        assert_eq!(verify_cades(b64.as_bytes()).unwrap().0, content);

        let mut bad = p7m.clone();
        let at = find(&bad, b"Consulenza").unwrap();
        bad[at] ^= 0x01;
        let e = verify_cades(&bad).unwrap_err();
        assert!(e.contains("changed after"), "{e}");
    }
}

#[test]
fn cades_p7m_accepted_by_openssl() {
    let openssl = require_tool!("openssl");
    let dir = tmp("cades-openssl");
    let ca = fixture_path("ca.pem");
    for (name, signer) in [("rsa", rsa_signer()), ("ec", ec_signer())] {
        let p7m = cades_p7m(FATTURA.as_bytes(), &signer, t()).unwrap();
        let f = dir.join(format!("{name}.xml.p7m"));
        fs::write(&f, &p7m).unwrap();
        let out_file = dir.join(format!("{name}.out.xml"));
        let o = run(Command::new(&openssl)
            .args(["cms", "-verify", "-binary", "-inform", "DER", "-in"])
            .arg(&f)
            .arg("-CAfile")
            .arg(&ca)
            .arg("-out")
            .arg(&out_file));
        assert!(
            o.status.success(),
            "openssl refused the {name} p7m: {}",
            text(&o)
        );
        assert!(text(&o).contains("Verification successful"));
        assert_eq!(fs::read(&out_file).unwrap(), FATTURA.as_bytes());

        // The CAdES attributes, as OpenSSL sees them.
        let o = run(Command::new(&openssl)
            .args(["cms", "-cmsout", "-print", "-inform", "DER", "-in"])
            .arg(&f));
        let printed = text(&o);
        for attr in [
            "contentType",
            "signingTime",
            "messageDigest",
            "id-smime-aa-signingCertificateV2",
        ] {
            assert!(printed.contains(attr), "missing {attr} in:\n{printed}");
        }

        let mut bad = p7m.clone();
        let at = find(&bad, b"Consulenza").unwrap();
        bad[at] ^= 0x01;
        let fb = dir.join(format!("{name}-tampered.p7m"));
        fs::write(&fb, &bad).unwrap();
        let o = run(Command::new(&openssl)
            .args(["cms", "-verify", "-binary", "-inform", "DER", "-in"])
            .arg(&fb)
            .arg("-CAfile")
            .arg(&ca)
            .arg("-out")
            .arg(dir.join("tampered.out")));
        assert!(!o.status.success(), "openssl accepted a tampered p7m");
    }
}

// ================================================================== PAdES

fn info() -> PadesInfo {
    PadesInfo {
        reason: Some("Fattura emessa".into()),
        location: Some("Milano".into()),
        name: Some("ACME S.p.A. — amministrazione".into()),
    }
}

#[test]
fn pades_round_trip_determinism_and_tampering() {
    for (pdf_name, pdf) in [
        ("classic xref", classic_pdf()),
        ("xref stream", xref_stream_pdf()),
    ] {
        for signer in [rsa_signer(), ec_signer()] {
            let signed =
                pades(&pdf, &signer, t(), &info()).unwrap_or_else(|e| panic!("{pdf_name}: {e}"));
            assert!(
                signed.starts_with(&pdf),
                "{pdf_name}: the original bytes are kept"
            );
            assert_eq!(
                signed,
                pades(&pdf, &signer, t(), &info()).unwrap(),
                "{pdf_name}: deterministic"
            );
            let v = verify_pades(&signed).unwrap_or_else(|e| panic!("{pdf_name}: {e}"));
            assert_eq!(v.subject, signer.subject());
            assert_eq!(v.signing_time, Some(t()), "from /M");
            assert!(v.covers_whole_file);

            let tail = String::from_utf8_lossy(&signed[pdf.len()..]).into_owned();
            assert!(tail.contains("/SubFilter/ETSI.CAdES.detached"), "{tail}");
            assert!(tail.contains("/M(D:20260601120000+00'00')"));
            if pdf_name == "xref stream" {
                assert!(
                    tail.contains("/Type /XRef"),
                    "the update uses an xref stream like the original"
                );
            } else {
                assert!(
                    tail.contains("\nxref\n"),
                    "the update uses a classic xref like the original"
                );
            }

            let mut bad = signed.clone();
            let at = find(&bad, b"Fattura 2026").unwrap();
            bad[at] = b'G';
            let e = verify_pades(&bad).unwrap_err();
            assert!(e.contains("changed after"), "{e}");
        }
    }
}

#[test]
fn pades_cms_has_no_signing_time_and_openssl_verifies_it() {
    let pdf = pades(&classic_pdf(), &rsa_signer(), t(), &info()).unwrap();
    let (cms, signed) = last_pdf_signature(&pdf);
    let openssl = require_tool!("openssl");
    let dir = tmp("pades-openssl");
    fs::write(dir.join("sig.der"), &cms).unwrap();
    fs::write(dir.join("signed.bin"), &signed).unwrap();
    let o = run(Command::new(&openssl)
        .args(["cms", "-verify", "-binary", "-inform", "DER", "-in"])
        .arg(dir.join("sig.der"))
        .arg("-content")
        .arg(dir.join("signed.bin"))
        .arg("-CAfile")
        .arg(fixture_path("ca.pem"))
        .arg("-out")
        .arg(dir.join("out.bin")));
    assert!(
        o.status.success(),
        "openssl refused the PAdES CMS: {}",
        text(&o)
    );
    let o = run(Command::new(&openssl)
        .args(["cms", "-cmsout", "-print", "-inform", "DER", "-in"])
        .arg(dir.join("sig.der")));
    let printed = text(&o);
    assert!(
        printed.contains("id-smime-aa-signingCertificateV2") && printed.contains("messageDigest")
    );
    assert!(
        !printed.contains("signingTime"),
        "PAdES puts the time in /M, not in the CMS"
    );
    assert!(printed.contains("eContent: <ABSENT>"), "detached");
}

fn pdfsig_check(
    pdfsig: &std::path::Path,
    dir: &std::path::Path,
    name: &str,
    pdf: &[u8],
    expect_valid: bool,
) {
    let f = dir.join(format!("{name}.pdf"));
    fs::write(&f, pdf).unwrap();
    let o = run(Command::new(pdfsig).arg(&f));
    let out = text(&o);
    if expect_valid {
        assert!(
            out.contains("Signature Validation: Signature is Valid."),
            "pdfsig on {name}:\n{out}"
        );
        assert!(
            !out.contains("Signature is Invalid") && !out.contains("Digest Mismatch"),
            "pdfsig on {name}:\n{out}"
        );
    } else {
        assert!(
            !out.contains("Signature is Valid."),
            "pdfsig accepted the tampered {name}:\n{out}"
        );
    }
}

#[test]
fn pades_accepted_by_pdfsig() {
    let pdfsig = require_tool!("pdfsig");
    let dir = tmp("pades-pdfsig");
    for (pdf_name, pdf) in [
        ("classic", classic_pdf()),
        ("xrefstream", xref_stream_pdf()),
    ] {
        for (key, signer) in [("rsa", rsa_signer()), ("ec", ec_signer())] {
            let signed = pades(&pdf, &signer, t(), &info()).unwrap();
            pdfsig_check(&pdfsig, &dir, &format!("{pdf_name}-{key}"), &signed, true);
            let mut bad = signed.clone();
            let at = find(&bad, b"Fattura 2026").unwrap();
            bad[at] = b'G';
            pdfsig_check(
                &pdfsig,
                &dir,
                &format!("{pdf_name}-{key}-tampered"),
                &bad,
                false,
            );
        }
    }
    if let Some(pdftotext) = tool("pdftotext") {
        let signed = pades(&xref_stream_pdf(), &rsa_signer(), t(), &info()).unwrap();
        let f = dir.join("text.pdf");
        fs::write(&f, &signed).unwrap();
        let o = run(Command::new(pdftotext).arg(&f).arg("-"));
        assert!(
            text(&o).contains("Fattura 2026/001"),
            "poppler still reads the signed PDF"
        );
    }
}

#[test]
fn pades_twice_both_signatures_valid() {
    let once = pades(&classic_pdf(), &rsa_signer(), t(), &PadesInfo::default()).unwrap();
    let twice = pades(&once, &ec_signer(), t(), &PadesInfo::default()).unwrap();
    let v = verify_pades(&twice).unwrap();
    assert_eq!(
        v.subject, "Beta S.r.l.",
        "the newest signature is described"
    );
    assert!(v.covers_whole_file);
    // The first signature covers only the first revision.
    let v1 = verify_pades(&once).unwrap();
    assert_eq!(v1.subject, "ACME S.p.A.");
    // Bytes appended after signing are not covered, and the result says so.
    let mut appended = once.clone();
    appended.extend_from_slice(b"% appended later\n");
    assert!(!verify_pades(&appended).unwrap().covers_whole_file);
    let tail = String::from_utf8_lossy(&twice[once.len()..]).into_owned();
    assert!(
        tail.contains("(Signature2)"),
        "the second field gets a new name: {tail}"
    );
    if let Some(pdfsig) = tool("pdfsig") {
        let dir = tmp("pades-twice");
        let f = dir.join("twice.pdf");
        fs::write(&f, &twice).unwrap();
        let out = text(&run(Command::new(pdfsig).arg(&f)));
        assert_eq!(out.matches("Signature is Valid.").count(), 2, "{out}");
    } else {
        eprintln!("note: `pdfsig` is not installed: skipping this external check");
    }
}

#[test]
fn pades_on_pdfs_made_by_other_software() {
    let dir = tmp("pades-real");
    let mut made = Vec::new();
    // poppler's pdfunite rewrites a PDF with its own writer.
    if let Some(pdfunite) = tool("pdfunite") {
        let a = dir.join("a.pdf");
        fs::write(&a, classic_pdf()).unwrap();
        let out = dir.join("united.pdf");
        let o = run(Command::new(pdfunite).arg(&a).arg(&a).arg(&out));
        if o.status.success() {
            made.push(("pdfunite", fs::read(&out).unwrap()));
        }
    }
    // macOS's Quartz PDF writer, through cupsfilter.
    if let Some(cupsfilter) = tool("cupsfilter") {
        let txt = dir.join("in.txt");
        fs::write(&txt, "Fattura 2026/001\nACME S.p.A.\n").unwrap();
        let o = run(Command::new(cupsfilter)
            .args(["-m", "application/pdf"])
            .arg(&txt));
        if o.status.success() && o.stdout.starts_with(b"%PDF-") {
            made.push(("cupsfilter", o.stdout.clone()));
        }
    }
    if made.is_empty() {
        eprintln!(
            "note: neither `pdfunite` nor `cupsfilter` is available: skipping this external check"
        );
        return;
    }
    for (name, pdf) in made {
        let signed =
            pades(&pdf, &rsa_signer(), t(), &info()).unwrap_or_else(|e| panic!("{name}: {e}"));
        verify_pades(&signed).unwrap_or_else(|e| panic!("{name}: {e}"));
        if let Some(pdfsig) = tool("pdfsig") {
            pdfsig_check(&pdfsig, &dir, name, &signed, true);
        }
    }
}

#[test]
fn pades_errors_say_where() {
    let s = rsa_signer();
    let e = pades(b"hello", &s, t(), &PadesInfo::default()).unwrap_err();
    assert!(e.contains("not a PDF"), "{e}");
    let mut broken = classic_pdf();
    let at = find(&broken, b"startxref\n").unwrap() + 10;
    broken.splice(at..at + 3, b"999".iter().copied());
    let e = pades(&broken, &s, t(), &PadesInfo::default()).unwrap_err();
    assert!(e.contains("byte"), "{e}");
    let mut broken = classic_pdf();
    let at = find(&broken, b"xref\n0 6").unwrap();
    broken[at] = b'X';
    let e = pades(&broken, &s, t(), &PadesInfo::default()).unwrap_err();
    assert!(e.contains(&format!("at byte {at}")), "{e}");
    let e = verify_pades(&classic_pdf()).unwrap_err();
    assert!(e.contains("no signature"), "{e}");
}

// ================================================================== XAdES

#[test]
fn xades_round_trip_determinism_and_tampering() {
    for signer in [rsa_signer(), ec_signer()] {
        let signed = xades_enveloped(FATTURA, &signer, t()).unwrap();
        assert_eq!(
            signed,
            xades_enveloped(FATTURA, &signer, t()).unwrap(),
            "deterministic"
        );
        let at = FATTURA.rfind("</p:FatturaElettronica>").unwrap();
        assert!(
            signed.starts_with(&FATTURA[..at])
                && signed.ends_with("</ds:Signature></p:FatturaElettronica>\n")
        );
        let v = verify_xades(&signed).unwrap();
        assert_eq!(v.subject, signer.subject());
        assert_eq!(v.signing_time, Some(t()));

        let bad = signed.replace("Consulenza</Descrizione>", "Consulenze</Descrizione>");
        let e = verify_xades(&bad).unwrap_err();
        assert!(e.contains("the XML document was changed"), "{e}");
        let bad = signed.replace("2026-06-01T12:00:00Z", "2026-06-02T12:00:00Z");
        let e = verify_xades(&bad).unwrap_err();
        assert!(e.contains("signedprops` was changed"), "{e}");
        let bad = signed.replacen("<ds:DigestValue>", "<ds:DigestValue>A", 1);
        assert!(verify_xades(&bad).is_err());
    }
    let e = xades_enveloped(
        &xades_enveloped(FATTURA, &rsa_signer(), t()).unwrap(),
        &rsa_signer(),
        t(),
    )
    .unwrap_err();
    assert!(e.contains("already signed"), "{e}");
    let e = xades_enveloped("<!DOCTYPE a><a/>", &rsa_signer(), t()).unwrap_err();
    assert!(e.contains("DTD"), "{e}");
    // A self-closing root works too.
    let s = xades_enveloped("<a xmlns=\"urn:x\"/>", &rsa_signer(), t()).unwrap();
    verify_xades(&s).unwrap();
}

/// Documents for comparing canonical forms with libxml2.
fn c14n_samples() -> Vec<(&'static str, String)> {
    vec![
        ("fattura", FATTURA.to_string()),
        ("signed fattura", xades_enveloped(FATTURA, &rsa_signer(), t()).unwrap()),
        (
            "namespaces and attributes",
            "<?xml version=\"1.0\"?>\r\n<r xmlns=\"urn:d\" xmlns:b=\"urn:b\" xmlns:a=\"urn:a\" z=\"1\" b:y=\"2\" a:y=\"3\" a=\"&lt;&amp;&quot;&gt;\">\r\n <a:x xmlns:a=\"urn:a\" xmlns:c=\"urn:c\" t='tab&#9;nl&#10;cr&#13;'><e/><f xmlns=\"\"><g xmlns=\"\"/></f></a:x>\r\n <![CDATA[ <cdata> & ]]>\r\n</r>".to_string(),
        ),
    ]
}

#[test]
fn c14n_matches_xmllint() {
    let xmllint = require_tool!("xmllint");
    let dir = tmp("c14n-xmllint");
    for (name, doc) in c14n_samples() {
        let f = dir.join("doc.xml");
        fs::write(&f, &doc).unwrap();
        let o = run(Command::new(&xmllint).arg("--c14n").arg(&f));
        assert!(o.status.success());
        let theirs = String::from_utf8(o.stdout).unwrap();
        let ours = canonicalize_xml(&doc).unwrap();
        assert_eq!(
            ours, theirs,
            "canonical form of `{name}` differs from xmllint --c14n"
        );
    }
}

#[test]
fn c14n_of_subtrees_matches_lxml() {
    // Inclusive C14N of a subtree carries every namespace in scope: the delicate part
    // of SignedInfo and SignedProperties. The lxml check below covers it on the signed
    // invoice; here the whole signed document is compared too.
    let Some(python) = python_with_lxml() else {
        eprintln!("note: python3 with lxml is not available: skipping this external check");
        return;
    };
    let dir = tmp("c14n-lxml");
    for (name, doc) in c14n_samples() {
        let f = dir.join("doc.xml");
        fs::write(&f, &doc).unwrap();
        let o = run(Command::new(&python).arg("-I").arg("-c").arg(
            "import sys; from lxml import etree; sys.stdout.buffer.write(etree.tostring(etree.parse(sys.argv[1]), method='c14n', exclusive=False, with_comments=False))",
        ).arg(&f));
        assert!(o.status.success());
        assert_eq!(
            canonicalize_xml(&doc).unwrap(),
            String::from_utf8(o.stdout).unwrap(),
            "`{name}`"
        );
    }
}

#[test]
fn xades_independent_check_with_lxml_and_openssl() {
    let Some(python) = python_with_lxml() else {
        eprintln!("note: python3 with lxml is not available: skipping this external check");
        return;
    };
    let openssl = require_tool!("openssl");
    for (name, signer) in [("rsa", rsa_signer()), ("ec", ec_signer())] {
        let dir = tmp(&format!("xades-lxml-{name}"));
        let signed = xades_enveloped(FATTURA, &signer, t()).unwrap();
        let f = dir.join("signed.xml");
        fs::write(&f, &signed).unwrap();
        let script = dir.join("check.py");
        fs::write(&script, XADES_CHECK_PY).unwrap();
        let o = run(Command::new(&python)
            .arg("-I")
            .arg(&script)
            .arg(&f)
            .arg(&dir));
        assert!(o.status.success(), "lxml digest check failed: {}", text(&o));
        let o = run(Command::new(&openssl)
            .args(["x509", "-inform", "DER", "-pubkey", "-noout", "-in"])
            .arg(dir.join("cert.der")));
        fs::write(dir.join("pub.pem"), &o.stdout).unwrap();
        let o = run(Command::new(&openssl)
            .args(["dgst", "-sha256", "-verify"])
            .arg(dir.join("pub.pem"))
            .arg("-signature")
            .arg(dir.join("signature.bin"))
            .arg(dir.join("signedinfo.c14n")));
        assert!(
            text(&o).contains("Verified OK"),
            "openssl did not verify SignedInfo ({name}): {}",
            text(&o)
        );

        let bad = signed.replace("Consulenza</Descrizione>", "Consulenze</Descrizione>");
        fs::write(&f, &bad).unwrap();
        let o = run(Command::new(&python)
            .arg("-I")
            .arg(&script)
            .arg(&f)
            .arg(&dir));
        assert!(
            !o.status.success() && text(&o).contains("DIGEST MISMATCH"),
            "lxml accepted a tampered document"
        );
    }
}

// ================================================================== OpenPGP

fn pgp_t() -> SigningTime {
    SigningTime::from_unix_seconds(PGP_T)
}

fn gpg_home(gpg: &std::path::Path, name: &str, keys: &[&str]) -> std::path::PathBuf {
    let dir = tmp(name);
    let home = dir.join("g");
    fs::create_dir_all(&home).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    }
    for (i, k) in keys.iter().enumerate() {
        let f = dir.join(format!("key{i}.asc"));
        fs::write(&f, k).unwrap();
        let o = run(Command::new(gpg)
            .arg("--homedir")
            .arg(&home)
            .args([
                "--batch",
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                "",
                "--import",
            ])
            .arg(&f));
        assert!(o.status.success(), "gpg --import failed");
    }
    home
}

fn gpg_done(gpg: &std::path::Path, home: &std::path::Path) {
    let gpgconf = gpg.with_file_name("gpgconf");
    let _ = Command::new(gpgconf)
        .arg("--homedir")
        .arg(home)
        .args(["--kill", "all"])
        .output();
}

#[test]
fn pgp_signature_round_trip_and_determinism() {
    let data = b"IBAN;IMPORTO\nIT60X0542811101000000123456;122.00\n";
    let a = pgp_sign_detached(data, PGP_SIGNER_SEC, Some(PGP_SIGNER_PASS), pgp_t()).unwrap();
    let b = pgp_sign_detached(data, PGP_SIGNER_SEC, Some(PGP_SIGNER_PASS), pgp_t()).unwrap();
    assert!(a.starts_with("-----BEGIN PGP SIGNATURE-----"));
    assert_eq!(a, b, "RSA signatures are deterministic");
    let e = pgp_sign_detached(data, PGP_SIGNER_SEC, Some("wrong"), pgp_t()).unwrap_err();
    assert!(e.contains("wrong passphrase"), "{e}");
    let e = pgp_sign_detached(data, PGP_SIGNER_SEC, None, pgp_t()).unwrap_err();
    assert!(e.contains("protected by a passphrase"), "{e}");
    let e = pgp_sign_detached(data, PGP_SIGNER_PUB, None, pgp_t()).unwrap_err();
    assert!(e.contains("public key"), "{e}");
    let e = pgp_sign_detached(data, PGP_SIGNER_SEC, Some(PGP_SIGNER_PASS), t()).unwrap_err();
    assert!(e.contains("after the signing time"), "{e}");
    // An Ed25519 key without a passphrase.
    let c = pgp_sign_detached(data, PGP_BANK_SEC, None, pgp_t()).unwrap();
    assert_eq!(
        c,
        pgp_sign_detached(data, PGP_BANK_SEC, None, pgp_t()).unwrap(),
        "Ed25519 is deterministic too"
    );
}

#[test]
fn pgp_signature_verified_by_gpg() {
    let gpg = require_tool!("gpg");
    let home = gpg_home(&gpg, "pgp-verify", &[PGP_SIGNER_PUB, PGP_BANK_PUB]);
    let dir = home.parent().unwrap().to_path_buf();
    let data = b"IBAN;IMPORTO\nIT60X0542811101000000123456;122.00\n";
    fs::write(dir.join("data.csv"), data).unwrap();
    for (name, key, pass) in [
        ("rsa", PGP_SIGNER_SEC, Some(PGP_SIGNER_PASS)),
        ("ed25519", PGP_BANK_SEC, None),
    ] {
        let sig = pgp_sign_detached(data, key, pass, pgp_t()).unwrap();
        fs::write(dir.join("data.csv.asc"), &sig).unwrap();
        let o = run(Command::new(&gpg)
            .arg("--homedir")
            .arg(&home)
            .args(["--batch", "--status-fd", "1", "--verify"])
            .arg(dir.join("data.csv.asc"))
            .arg(dir.join("data.csv")));
        assert!(
            o.status.success() && text(&o).contains("[GNUPG:] GOODSIG"),
            "gpg refused the {name} signature"
        );
        fs::write(
            dir.join("data.csv"),
            b"IBAN;IMPORTO\nIT60X0542811101000000123456;922.00\n",
        )
        .unwrap();
        let o = run(Command::new(&gpg)
            .arg("--homedir")
            .arg(&home)
            .args(["--batch", "--status-fd", "1", "--verify"])
            .arg(dir.join("data.csv.asc"))
            .arg(dir.join("data.csv")));
        assert!(
            !o.status.success() && text(&o).contains("BADSIG"),
            "gpg accepted tampered data"
        );
        fs::write(dir.join("data.csv"), data).unwrap();
    }
    gpg_done(&gpg, &home);
}

#[test]
fn pgp_encryption_decrypted_by_gpg() {
    let data = b"<Document>pagamenti</Document>\n".repeat(50);
    let plain = pgp_encrypt(&data, &[PGP_BANK_PUB], None, pgp_t()).unwrap();
    let signed = pgp_encrypt(
        &data,
        &[PGP_BANK_PUB, PGP_SIGNER_PUB],
        Some((PGP_SIGNER_SEC, Some(PGP_SIGNER_PASS))),
        pgp_t(),
    )
    .unwrap();
    let armored = pgp_encrypt_armored(&data, &[PGP_BANK_PUB], None, pgp_t()).unwrap();
    assert!(armored.starts_with("-----BEGIN PGP MESSAGE-----"));
    assert_ne!(
        plain,
        pgp_encrypt(&data, &[PGP_BANK_PUB], None, pgp_t()).unwrap(),
        "fresh session key each time"
    );
    let e = pgp_encrypt(&data, &[], None, pgp_t()).unwrap_err();
    assert!(e.contains("no recipient"), "{e}");
    let e = pgp_encrypt(&data, &["garbage"], None, pgp_t()).unwrap_err();
    assert!(e.contains("recipient 1"), "{e}");

    let gpg = require_tool!("gpg");
    let home = gpg_home(&gpg, "pgp-decrypt", &[PGP_BANK_SEC, PGP_SIGNER_PUB]);
    let dir = home.parent().unwrap().to_path_buf();
    for (name, msg) in [
        ("plain.gpg", plain),
        ("signed.gpg", signed),
        ("armored.asc", armored.into_bytes()),
    ] {
        let f = dir.join(name);
        fs::write(&f, &msg).unwrap();
        let o = run(Command::new(&gpg)
            .arg("--homedir")
            .arg(&home)
            .args([
                "--batch",
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                "",
                "--status-fd",
                "2",
                "--decrypt",
            ])
            .arg(&f));
        assert!(o.status.success(), "gpg could not decrypt {name}");
        assert_eq!(o.stdout, data, "{name}");
        if name == "signed.gpg" {
            assert!(
                String::from_utf8_lossy(&o.stderr).contains("[GNUPG:] GOODSIG"),
                "the signature inside is valid"
            );
        }
    }
    gpg_done(&gpg, &home);
}

// ================================================================== helpers

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn base64_encode(b: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}
