//! GS1 element strings: parsing the human form `(01)09501101530003(17)261231(10)LOT42`,
//! checking each application identifier (AI) and its value, and the GS1 check digit.

/// One GS1 element string: an application identifier and its value, both as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ElementString {
    pub ai: String,
    pub value: String,
}

/// What a value of an AI may hold.
#[derive(Clone, Copy)]
enum Format {
    /// Exactly n digits.
    Digits(usize),
    /// 1 to n digits.
    DigitsUpTo(usize),
    /// 1 to n characters of GS1 set 82.
    TextUpTo(usize),
    /// A date YYMMDD.
    Date,
    /// AI 421: three digits (ISO country code) then up to 9 characters of set 82.
    CountryAndPostal,
}

struct AiSpec {
    format: Format,
    /// The last digit is a GS1 check digit.
    check_digit: bool,
    title: &'static str,
}

/// The application identifiers this crate validates.
pub(crate) const SUPPORTED: &str = "00, 01, 02, 10, 11, 13, 15, 17, 20, 21, 30, 37, 3100-3699, \
400-403, 410-415, 420, 421";

fn spec(ai: &str) -> Option<AiSpec> {
    use Format::*;
    let s = |format, check_digit, title| {
        Some(AiSpec {
            format,
            check_digit,
            title,
        })
    };
    match ai {
        "00" => s(Digits(18), true, "SSCC"),
        "01" => s(Digits(14), true, "GTIN"),
        "02" => s(Digits(14), true, "GTIN of contained trade items"),
        "10" => s(TextUpTo(20), false, "batch or lot number"),
        "11" => s(Date, false, "production date"),
        "13" => s(Date, false, "packaging date"),
        "15" => s(Date, false, "best before date"),
        "17" => s(Date, false, "expiration date"),
        "20" => s(Digits(2), false, "internal product variant"),
        "21" => s(TextUpTo(20), false, "serial number"),
        "30" => s(DigitsUpTo(8), false, "variable count"),
        "37" => s(DigitsUpTo(8), false, "count of trade items"),
        "400" => s(TextUpTo(30), false, "customer's purchase order number"),
        "401" => s(
            TextUpTo(30),
            false,
            "global identification number for consignment",
        ),
        "402" => s(Digits(17), true, "global shipment identification number"),
        "403" => s(TextUpTo(30), false, "routing code"),
        "410" => s(Digits(13), true, "ship to GLN"),
        "411" => s(Digits(13), true, "bill to GLN"),
        "412" => s(Digits(13), true, "purchased from GLN"),
        "413" => s(Digits(13), true, "ship for GLN"),
        "414" => s(Digits(13), true, "GLN of a physical location"),
        "415" => s(Digits(13), true, "GLN of the invoicing party"),
        "420" => s(TextUpTo(20), false, "ship to postal code"),
        "421" => s(
            CountryAndPostal,
            false,
            "ship to postal code with ISO country code",
        ),
        _ if ai.len() == 4 && ai.bytes().all(|b| b.is_ascii_digit()) => {
            let head: u32 = ai[..3].parse().ok()?;
            if (310..=369).contains(&head) {
                s(Digits(6), false, "trade measure")
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Whether an element string with this AI is followed by a separator (FNC1) when another
/// element string comes after it. GS1 fixes the length of the AIs starting with these two
/// digits (General Specifications, figure 5.10.1-2): every other AI needs the separator,
/// even when its own length is fixed (402, 410-415).
pub(crate) fn needs_separator(ai: &str) -> bool {
    const PREDEFINED: [&str; 22] = [
        "00", "01", "02", "03", "04", "11", "12", "13", "14", "15", "16", "17", "18", "19", "20",
        "31", "32", "33", "34", "35", "36", "41",
    ];
    !PREDEFINED.contains(&&ai[..2])
}

/// The characters GS1 allows in alphanumeric values (set 82), except `(` which this crate
/// reads as the start of the next application identifier.
fn in_set_82(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!\"%&')*+,-./:;<=>?_".contains(c)
}

/// The GS1 check digit of `digits` (all the digits that come before the check digit):
/// from the right, digits are weighted 3, 1, 3, 1, ...; the check digit brings the sum to a
/// multiple of ten. The same rule serves GTIN, SSCC, GLN and GSIN.
///
/// ```
/// assert_eq!(quadra_label::check_digit("400638133393"), Ok(1));
/// assert_eq!(quadra_label::check_digit("37610425002123456"), Ok(9));
/// ```
pub fn check_digit(digits: &str) -> Result<u8, String> {
    if digits.is_empty() {
        return Err("a check digit needs at least one digit".into());
    }
    let mut sum = 0u32;
    for (i, c) in digits.chars().rev().enumerate() {
        let d = c
            .to_digit(10)
            .ok_or_else(|| format!("a check digit is computed on digits only, not `{c}`"))?;
        sum += if i % 2 == 0 { d * 3 } else { d };
    }
    Ok(((10 - sum % 10) % 10) as u8)
}

fn check_date(ai: &str, v: &str) -> Result<(), String> {
    let month: u32 = v[2..4].parse().unwrap_or(0);
    let day: u32 = v[4..6].parse().unwrap_or(0);
    if !(1..=12).contains(&month) {
        return Err(format!(
            "({ai}) is a date YYMMDD; `{v}` has month {:02}",
            month
        ));
    }
    let max_day = match month {
        2 => 29,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    // Day 00 is allowed by GS1: it means the last day of the month.
    if day > max_day {
        return Err(format!(
            "({ai}) is a date YYMMDD; `{v}` has day {day:02}, and month {month:02} has {max_day} days"
        ));
    }
    Ok(())
}

fn digits_only(ai: &str, v: &str) -> Result<(), String> {
    if let Some((i, c)) = v.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        return Err(format!(
            "the value of ({ai}) takes digits only; `{c}` at position {} of `{v}` is not a digit",
            i + 1
        ));
    }
    Ok(())
}

fn text_only(ai: &str, v: &str) -> Result<(), String> {
    if let Some(c) = v.chars().find(|c| !in_set_82(*c)) {
        return Err(format!(
            "`{c}` is not allowed in the value of ({ai}): GS1 allows letters A-Z and a-z, \
digits and ! \" % & ' ) * + , - . / : ; < = > ? _"
        ));
    }
    Ok(())
}

fn check_value(ai: &str, v: &str) -> Result<(), String> {
    let spec = spec(ai).ok_or_else(|| {
        format!("application identifier ({ai}) is not supported (supported: {SUPPORTED})")
    })?;
    let title = spec.title;
    if v.is_empty() {
        return Err(format!("({ai}) ({title}) has no value"));
    }
    let n = v.chars().count();
    match spec.format {
        Format::Digits(len) => {
            digits_only(ai, v)?;
            if n != len {
                return Err(format!(
                    "the value of ({ai}) ({title}) must be {len} digits, not {n}: `{v}`"
                ));
            }
        }
        Format::DigitsUpTo(max) => {
            digits_only(ai, v)?;
            if n > max {
                return Err(format!(
                    "({ai}) ({title}) holds at most {max} digits, not {n}: `{v}`"
                ));
            }
        }
        Format::TextUpTo(max) => {
            text_only(ai, v)?;
            if n > max {
                return Err(format!(
                    "({ai}) ({title}) holds at most {max} characters, not {n}: `{v}`"
                ));
            }
        }
        Format::Date => {
            digits_only(ai, v)?;
            if n != 6 {
                return Err(format!(
                    "({ai}) ({title}) is a date YYMMDD: 6 digits, not {n}: `{v}`"
                ));
            }
            check_date(ai, v)?;
        }
        Format::CountryAndPostal => {
            text_only(ai, v)?;
            if !(4..=12).contains(&n) || !v[..3].bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!(
                    "({ai}) ({title}) is a 3-digit ISO country code followed by up to 9 \
characters of postal code: `{v}`"
                ));
            }
        }
    }
    if spec.check_digit {
        let (body, last) = v.split_at(v.len() - 1);
        let want = check_digit(body)?;
        if last != want.to_string() {
            return Err(format!("the check digit of ({ai}) `{v}` should be {want}"));
        }
    }
    Ok(())
}

/// Reads GS1 element strings in human form: each application identifier in parentheses
/// followed by its value, `(01)09501101530003(17)261231(10)LOT42`. A value runs to the next
/// `(` or to the end, so values cannot hold `(`.
pub(crate) fn parse(data: &str) -> Result<Vec<ElementString>, String> {
    let mut out: Vec<ElementString> = Vec::new();
    if !data.starts_with('(') {
        return Err(format!(
            "GS1 data starts with an application identifier in parentheses, like \
`(01)09501101530003(10)LOT42`, not `{}`",
            data.chars().take(12).collect::<String>()
        ));
    }
    let mut rest = data;
    while !rest.is_empty() {
        let after = &rest[1..];
        let close = after
            .find(')')
            .ok_or_else(|| format!("GS1: the `(` at `{}` is never closed", short(rest)))?;
        let ai = &after[..close];
        if ai.len() < 2 || ai.len() > 4 || !ai.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "GS1: `({ai})` is not an application identifier (2 to 4 digits in parentheses)"
            ));
        }
        let tail = &after[close + 1..];
        let end = tail.find('(').unwrap_or(tail.len());
        let value = &tail[..end];
        check_value(ai, value).map_err(|e| format!("GS1: {e}"))?;
        if out.iter().any(|e| e.ai == ai) {
            return Err(format!("GS1: application identifier ({ai}) appears twice"));
        }
        out.push(ElementString {
            ai: ai.to_string(),
            value: value.to_string(),
        });
        rest = &tail[end..];
    }
    Ok(out)
}

fn short(s: &str) -> String {
    s.chars().take(16).collect()
}

/// The human-readable form, `(01)09501101530003(17)261231`.
pub(crate) fn human(elements: &[ElementString]) -> String {
    elements
        .iter()
        .map(|e| format!("({}){}", e.ai, e.value))
        .collect()
}

/// The SSCC element string from 17 digits (the check digit is computed) or 18 (verified).
pub(crate) fn sscc(data: &str) -> Result<Vec<ElementString>, String> {
    if let Some((i, c)) = data.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        return Err(format!(
            "SSCC takes digits only; `{c}` at position {} is not a digit",
            i + 1
        ));
    }
    let n = data.len();
    let value = match n {
        17 => format!("{data}{}", check_digit(data)?),
        18 => {
            let want = check_digit(&data[..17])?;
            if data[17..] != want.to_string() {
                return Err(format!("the check digit of SSCC `{data}` should be {want}"));
            }
            data.to_string()
        }
        _ => {
            return Err(format!(
                "SSCC takes 17 digits (extension digit, company prefix and serial reference) \
or 18 with the check digit, not {n}"
            ))
        }
    };
    Ok(vec![ElementString {
        ai: "00".into(),
        value,
    }])
}

/// A piece of GS1 data as a barcode carries it: FNC1 or a character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unit {
    Fnc1,
    Byte(u8),
}

/// The units of a GS1 symbol: FNC1 first, the element strings, and FNC1 after each element
/// string of variable length that is not the last.
pub(crate) fn units(elements: &[ElementString]) -> Vec<Unit> {
    let mut out = vec![Unit::Fnc1];
    for (i, e) in elements.iter().enumerate() {
        out.extend(e.ai.bytes().map(Unit::Byte));
        out.extend(e.value.bytes().map(Unit::Byte));
        if i + 1 < elements.len() && needs_separator(&e.ai) {
            out.push(Unit::Fnc1);
        }
    }
    out
}
