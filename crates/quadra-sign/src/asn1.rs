//! A small BER reader and DER writer, enough for CMS and PKCS#12.
//!
//! The reader accepts definite and indefinite lengths and constructed OCTET STRINGs,
//! because `.p7m` and `.p12` files made by other tools are often BER. The writer
//! always produces DER.

use der::oid::ObjectIdentifier;
use std::borrow::Cow;

pub(crate) const SEQUENCE: u8 = 0x30;
pub(crate) const SET: u8 = 0x31;
pub(crate) const INTEGER: u8 = 0x02;
pub(crate) const OCTET_STRING: u8 = 0x04;
pub(crate) const NULL: u8 = 0x05;
pub(crate) const OID: u8 = 0x06;
pub(crate) const UTC_TIME: u8 = 0x17;
pub(crate) const GENERALIZED_TIME: u8 = 0x18;

/// One element read from BER input.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tlv<'a> {
    /// The identifier octet (only low tag numbers are supported).
    pub tag: u8,
    /// The contents octets. For an indefinite length, everything before the end-of-contents.
    pub content: &'a [u8],
    /// The whole element, header included.
    pub raw: &'a [u8],
}

impl<'a> Tlv<'a> {
    pub fn is_constructed(&self) -> bool {
        self.tag & 0x20 != 0
    }

    /// The elements inside a constructed element.
    pub fn children(&self) -> Result<Vec<Tlv<'a>>, String> {
        if !self.is_constructed() {
            return Err(format!(
                "expected a constructed element, found tag 0x{:02x}",
                self.tag
            ));
        }
        read_all(self.content)
    }

    /// The elements inside, checking the tag first.
    pub fn expect(&self, tag: u8, what: &str) -> Result<Vec<Tlv<'a>>, String> {
        if self.tag != tag {
            return Err(format!(
                "{what}: expected tag 0x{tag:02x}, found 0x{:02x}",
                self.tag
            ));
        }
        self.children()
    }

    /// The bytes of an OCTET STRING, joining the pieces of a constructed one.
    pub fn octets(&self) -> Result<Cow<'a, [u8]>, String> {
        match self.tag {
            OCTET_STRING => Ok(Cow::Borrowed(self.content)),
            0x24 => {
                let mut out = Vec::new();
                for part in self.children()? {
                    out.extend_from_slice(&part.octets()?);
                }
                Ok(Cow::Owned(out))
            }
            t => Err(format!("expected an OCTET STRING, found tag 0x{t:02x}")),
        }
    }

    /// True when this is an OBJECT IDENTIFIER equal to `oid`.
    pub fn is_oid(&self, oid: &ObjectIdentifier) -> bool {
        self.tag == OID && self.content == oid.as_bytes()
    }

    /// The OBJECT IDENTIFIER, as dotted text (for error messages).
    pub fn oid_string(&self) -> String {
        if self.tag != OID {
            return format!("(not an OID: tag 0x{:02x})", self.tag);
        }
        ObjectIdentifier::from_bytes(self.content)
            .map(|o| o.to_string())
            .unwrap_or_else(|_| "(malformed OID)".into())
    }

    /// The value of a small non-negative INTEGER.
    pub fn small_uint(&self) -> Result<u64, String> {
        if self.tag != INTEGER || self.content.is_empty() || self.content.len() > 9 {
            return Err("expected a small INTEGER".into());
        }
        if self.content[0] & 0x80 != 0 {
            return Err("expected a non-negative INTEGER".into());
        }
        let mut v: u64 = 0;
        for &b in self.content {
            v = v.checked_mul(256).ok_or("INTEGER too large")? | b as u64;
        }
        Ok(v)
    }
}

/// Reads one element from the start of `input`, returning it and the rest.
pub(crate) fn read_one(input: &[u8]) -> Result<(Tlv<'_>, &[u8]), String> {
    read_one_depth(input, 0)
}

fn read_one_depth(input: &[u8], depth: usize) -> Result<(Tlv<'_>, &[u8]), String> {
    if depth > 64 {
        return Err("ASN.1 nesting too deep".into());
    }
    let truncated = || "truncated ASN.1 data".to_string();
    let tag = *input.first().ok_or_else(truncated)?;
    if tag & 0x1f == 0x1f {
        return Err("ASN.1 tags with high tag numbers are not supported".into());
    }
    let first = *input.get(1).ok_or_else(truncated)?;
    let mut pos = 2;
    if first == 0x80 {
        // Indefinite length: children until the end-of-contents octets.
        if tag & 0x20 == 0 {
            return Err("indefinite length on a primitive ASN.1 element".into());
        }
        let start = pos;
        loop {
            let rest = &input[pos..];
            if rest.len() >= 2 && rest[0] == 0 && rest[1] == 0 {
                let content = &input[start..pos];
                let raw = &input[..pos + 2];
                return Ok((Tlv { tag, content, raw }, &input[pos + 2..]));
            }
            let (_, after) = read_one_depth(rest, depth + 1)?;
            pos = input.len() - after.len();
        }
    }
    let len = if first < 0x80 {
        first as usize
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return Err("unsupported ASN.1 length".into());
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *input.get(pos + i).ok_or_else(truncated)? as usize;
        }
        pos += n;
        len
    };
    let end = pos
        .checked_add(len)
        .filter(|&e| e <= input.len())
        .ok_or_else(truncated)?;
    Ok((
        Tlv {
            tag,
            content: &input[pos..end],
            raw: &input[..end],
        },
        &input[end..],
    ))
}

/// Reads every element in `input`.
pub(crate) fn read_all(mut input: &[u8]) -> Result<Vec<Tlv<'_>>, String> {
    let mut out = Vec::new();
    while !input.is_empty() {
        let (t, rest) = read_one(input)?;
        out.push(t);
        input = rest;
    }
    Ok(out)
}

/// Reads exactly one element covering all of `input` (trailing zero padding is allowed,
/// as in a PDF `/Contents` string).
pub(crate) fn read_whole(input: &[u8]) -> Result<Tlv<'_>, String> {
    let (t, rest) = read_one(input)?;
    if rest.iter().any(|&b| b != 0) {
        return Err("unexpected data after the ASN.1 structure".into());
    }
    Ok(t)
}

// ---------------------------------------------------------------- writer

/// Encodes one DER element.
pub(crate) fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 6);
    out.push(tag);
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

/// A constructed element made of already encoded parts.
pub(crate) fn cons(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    tlv(tag, &parts.concat())
}

pub(crate) fn seq(parts: &[&[u8]]) -> Vec<u8> {
    cons(SEQUENCE, parts)
}

/// A DER SET OF: the elements sorted by their encodings.
pub(crate) fn set_of(mut parts: Vec<Vec<u8>>) -> Vec<u8> {
    parts.sort_by(|a, b| der_set_order(a, b));
    tlv(SET, &parts.concat())
}

/// X.690 11.6: compare as octet strings, the shorter padded with zero octets.
fn der_set_order(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x.cmp(&y);
        }
    }
    std::cmp::Ordering::Equal
}

pub(crate) fn oid(oid: &ObjectIdentifier) -> Vec<u8> {
    tlv(OID, oid.as_bytes())
}

pub(crate) fn null() -> Vec<u8> {
    vec![NULL, 0]
}

pub(crate) fn octet_string(bytes: &[u8]) -> Vec<u8> {
    tlv(OCTET_STRING, bytes)
}

pub(crate) fn small_int(v: u64) -> Vec<u8> {
    let bytes = v.to_be_bytes();
    let skip = bytes.iter().take_while(|&&b| b == 0).count().min(7);
    let mut content = bytes[skip..].to_vec();
    if content[0] & 0x80 != 0 {
        content.insert(0, 0);
    }
    tlv(INTEGER, &content)
}

/// An AlgorithmIdentifier, with parameters (already encoded) or without.
pub(crate) fn algorithm(id: &ObjectIdentifier, params: Option<&[u8]>) -> Vec<u8> {
    match params {
        Some(p) => seq(&[&oid(id), p]),
        None => seq(&[&oid(id)]),
    }
}

/// `[n] EXPLICIT` around an encoded element.
pub(crate) fn explicit(n: u8, inner: &[u8]) -> Vec<u8> {
    tlv(0xa0 | n, inner)
}

/// Turns an encoded element into `[n] IMPLICIT` with the same contents.
pub(crate) fn implicit_constructed(n: u8, encoded: &[u8]) -> Vec<u8> {
    let mut out = encoded.to_vec();
    out[0] = 0xa0 | n;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_and_ints() {
        assert_eq!(tlv(4, &[1, 2]), vec![4, 2, 1, 2]);
        let long = vec![0u8; 300];
        let enc = tlv(4, &long);
        assert_eq!(&enc[..4], &[4, 0x82, 0x01, 0x2c]);
        assert_eq!(small_int(0), vec![2, 1, 0]);
        assert_eq!(small_int(1), vec![2, 1, 1]);
        assert_eq!(small_int(128), vec![2, 2, 0, 128]);
        assert_eq!(small_int(256), vec![2, 2, 1, 0]);
        let (t, rest) = read_one(&enc).unwrap();
        assert!(rest.is_empty());
        assert_eq!(t.content.len(), 300);
    }

    #[test]
    fn indefinite_length() {
        // SEQUENCE (indefinite) { constructed OCTET STRING (indefinite) { "ab", "c" } }
        let data = [
            0x30, 0x80, 0x24, 0x80, 4, 2, b'a', b'b', 4, 1, b'c', 0, 0, 0, 0, 9,
        ];
        let (t, rest) = read_one(&data).unwrap();
        assert_eq!(rest, &[9]);
        let kids = t.children().unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(&*kids[0].octets().unwrap(), b"abc");
    }

    #[test]
    fn set_order() {
        let s = set_of(vec![vec![4, 1, 2], vec![4, 1, 1], vec![2, 1, 9]]);
        assert_eq!(s, vec![0x31, 9, 2, 1, 9, 4, 1, 1, 4, 1, 2]);
    }
}
