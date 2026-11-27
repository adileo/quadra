//! Code 128 (ISO/IEC 15417), also the carrier of GS1-128: the choice of code sets with the
//! fewest symbols, the check symbol and the bars.

use crate::gs1::Unit;

/// The three code sets of Code 128.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Set {
    A,
    B,
    C,
}

/// One symbol character of the plan, before the check symbol and the stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Token {
    Start(Set),
    Switch(Set),
    Fnc1,
    /// A character in code set A or B (its byte value).
    Char(u8),
    /// Two digits in code set C (00-99).
    Pair(u8),
}

/// Bar and space widths of the 107 symbol characters (value 106 is the stop, 7 elements).
const PATTERNS: [&str; 107] = [
    "212222", "222122", "222221", "121223", "121322", "131222", "122213", "122312", "132212",
    "221213", "221312", "231212", "112232", "122132", "122231", "113222", "123122", "123221",
    "223211", "221132", "221231", "213212", "223112", "312131", "311222", "321122", "321221",
    "312212", "322112", "322211", "212123", "212321", "232121", "111323", "131123", "131321",
    "112313", "132113", "132311", "211313", "231113", "231311", "112133", "112331", "132131",
    "113123", "113321", "133121", "313121", "211331", "231131", "213113", "213311", "213131",
    "311123", "311321", "331121", "312113", "312311", "332111", "314111", "221411", "431111",
    "111224", "111422", "121124", "121421", "141122", "141221", "112214", "112412", "122114",
    "122411", "142112", "142211", "241211", "221114", "413111", "241112", "134111", "111242",
    "121142", "121241", "114212", "124112", "124211", "411212", "421112", "421211", "212141",
    "214121", "412121", "111143", "111341", "131141", "114113", "114311", "411113", "411311",
    "113141", "114131", "311141", "411131", "211412", "211214", "211232", "2331112",
];

const INF: usize = usize::MAX / 4;

fn fits(set: Set, units: &[Unit], i: usize) -> Option<usize> {
    let digit = |k: usize| matches!(units.get(k), Some(Unit::Byte(b)) if b.is_ascii_digit());
    match (set, units[i]) {
        (_, Unit::Fnc1) => Some(1),
        (Set::C, Unit::Byte(_)) => (digit(i) && digit(i + 1)).then_some(2),
        (Set::A, Unit::Byte(b)) => (b < 96).then_some(1),
        (Set::B, Unit::Byte(b)) => (32..128).contains(&b).then_some(1),
    }
}

const SETS: [Set; 3] = [Set::C, Set::B, Set::A];

fn idx(s: Set) -> usize {
    match s {
        Set::C => 0,
        Set::B => 1,
        Set::A => 2,
    }
}

/// The plan with the fewest symbol characters (a shortest path over positions and code
/// sets). Ties prefer staying in the current set, then C, B, A, so the result is always the
/// same for the same input.
pub(crate) fn plan(units: &[Unit]) -> Result<Vec<Token>, String> {
    if units.is_empty() {
        return Err("Code 128 needs at least one character".into());
    }
    for (i, u) in units.iter().enumerate() {
        if let Unit::Byte(b) = u {
            if *b >= 128 {
                return Err(format!(
                    "Code 128 holds ASCII characters only; the character at byte {} is not",
                    i + 1
                ));
            }
        }
    }
    let n = units.len();
    // cost[i][s]: symbols to encode units[i..] when set s is active.
    let mut cost = vec![[INF; 3]; n + 1];
    cost[n] = [0; 3];
    // direct[i][s]: symbols to encode units[i..] when the next unit is encoded in s.
    let mut direct = vec![[INF; 3]; n + 1];
    for i in (0..n).rev() {
        for s in SETS {
            if let Some(k) = fits(s, units, i) {
                direct[i][idx(s)] = 1 + cost[i + k][idx(s)];
            }
        }
        for s in SETS {
            let mut best = direct[i][idx(s)];
            for t in SETS {
                if t != s && direct[i][idx(t)] < INF {
                    best = best.min(1 + direct[i][idx(t)]);
                }
            }
            cost[i][idx(s)] = best;
        }
    }
    let mut start = Set::C;
    for s in SETS {
        if direct[0][idx(s)] < direct[0][idx(start)] {
            start = s;
        }
    }
    let mut tokens = vec![Token::Start(start)];
    let mut set = start;
    let mut i = 0;
    while i < n {
        if direct[i][idx(set)] != cost[i][idx(set)] {
            let mut next = None;
            for t in SETS {
                if t != set && direct[i][idx(t)] < INF && 1 + direct[i][idx(t)] == cost[i][idx(set)]
                {
                    next = Some(t);
                    break;
                }
            }
            set = next.expect("a set that encodes the next unit");
            tokens.push(Token::Switch(set));
        }
        let k = fits(set, units, i).expect("the chosen set encodes the unit");
        tokens.push(match units[i] {
            Unit::Fnc1 => Token::Fnc1,
            Unit::Byte(b) if set == Set::C => {
                let Unit::Byte(b2) = units[i + 1] else {
                    unreachable!()
                };
                Token::Pair((b - b'0') * 10 + (b2 - b'0'))
            }
            Unit::Byte(b) => Token::Char(b),
        });
        i += k;
    }
    Ok(tokens)
}

/// The symbol values of a plan, with the check symbol and the stop.
pub(crate) fn values(tokens: &[Token]) -> Vec<u8> {
    let mut set = Set::B;
    let mut out = Vec::with_capacity(tokens.len() + 2);
    for t in tokens {
        out.push(match *t {
            Token::Start(s) => {
                set = s;
                match s {
                    Set::A => 103,
                    Set::B => 104,
                    Set::C => 105,
                }
            }
            Token::Switch(s) => {
                set = s;
                match s {
                    Set::C => 99,
                    Set::B => 100,
                    Set::A => 101,
                }
            }
            Token::Fnc1 => 102,
            Token::Pair(p) => p,
            Token::Char(b) => match set {
                Set::A if b < 32 => b + 64,
                _ => b - 32,
            },
        });
    }
    let mut sum = out[0] as u32;
    for (k, v) in out.iter().enumerate().skip(1) {
        sum += k as u32 * *v as u32;
    }
    out.push((sum % 103) as u8);
    out.push(106);
    out
}

/// The modules (true = bar) of the symbol values, without quiet zones.
pub(crate) fn bars(values: &[u8]) -> Vec<bool> {
    let mut out = Vec::new();
    for v in values {
        for (k, w) in PATTERNS[*v as usize].bytes().enumerate() {
            let dark = k % 2 == 0;
            out.extend(std::iter::repeat_n(dark, (w - b'0') as usize));
        }
    }
    out
}

/// The units of plain text.
pub(crate) fn text_units(data: &str) -> Result<Vec<Unit>, String> {
    if let Some((i, c)) = data.char_indices().find(|(_, c)| !c.is_ascii()) {
        return Err(format!(
            "Code 128 holds ASCII characters only; `{c}` at position {} is not",
            data[..i].chars().count() + 1
        ));
    }
    Ok(data.bytes().map(Unit::Byte).collect())
}
