//! EAN-13 (ISO/IEC 15420).

use crate::gs1::check_digit;

/// Set A (odd parity) patterns of the digits; set B is the mirror of set C, set C the
/// complement of set A.
const L: [&str; 10] = [
    "0001101", "0011001", "0010011", "0111101", "0100011", "0110001", "0101111", "0111011",
    "0110111", "0001011",
];

/// The set (A or B) of each of the six left digits, by the first digit.
const PARITY: [&str; 10] = [
    "AAAAAA", "AABABB", "AABBAB", "AABBBA", "ABAABB", "ABBAAB", "ABBBAA", "ABABAB", "ABABBA",
    "ABBABA",
];

/// The 13 digits: 12 given (the check digit is computed) or 13 (the check digit is verified).
pub(crate) fn digits(data: &str) -> Result<String, String> {
    if let Some((i, c)) = data.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        return Err(format!(
            "EAN-13 takes digits only; `{c}` at position {} is not a digit",
            data[..i].chars().count() + 1
        ));
    }
    match data.len() {
        12 => Ok(format!("{data}{}", check_digit(data)?)),
        13 => {
            let want = check_digit(&data[..12])?;
            if data[12..] != want.to_string() {
                return Err(format!("the check digit of `{data}` should be {want}"));
            }
            Ok(data.to_string())
        }
        n => Err(format!("EAN-13 takes 12 or 13 digits, not {n}")),
    }
}

/// The 95 modules of the 13 digits.
pub(crate) fn bars(digits: &str) -> Vec<bool> {
    let d: Vec<usize> = digits.bytes().map(|b| (b - b'0') as usize).collect();
    let mut s = String::from("101");
    for (k, parity) in PARITY[d[0]].chars().enumerate() {
        let a = L[d[k + 1]];
        if parity == 'A' {
            s.push_str(a);
        } else {
            // Set B: the set C pattern read backwards; set C is set A inverted.
            s.extend(a.chars().rev().map(|c| if c == '0' { '1' } else { '0' }));
        }
    }
    s.push_str("01010");
    for &digit in &d[7..13] {
        s.extend(L[digit].chars().map(|c| if c == '0' { '1' } else { '0' }));
    }
    s.push_str("101");
    s.chars().map(|c| c == '1').collect()
}
