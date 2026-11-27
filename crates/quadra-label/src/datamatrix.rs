//! Data Matrix ECC 200 (ISO/IEC 16022), square symbols, ASCII encodation.
//!
//! ASCII encodation packs two digits in one codeword, which is what GS1 data (mostly
//! digits) needs; FNC1 is codeword 232 in first position (GS1 Data Matrix) and as the
//! separator after variable-length element strings.

use crate::gs1::Unit;

/// A square ECC 200 symbol size.
struct Size {
    /// Modules per side, finder patterns included.
    side: usize,
    data: usize,
    ecc: usize,
    /// Data regions per side.
    regions: usize,
    /// Interleaved Reed-Solomon blocks.
    blocks: usize,
}

const fn sz(side: usize, data: usize, ecc: usize, regions: usize, blocks: usize) -> Size {
    Size {
        side,
        data,
        ecc,
        regions,
        blocks,
    }
}

/// ISO/IEC 16022 table 7, square sizes.
const SIZES: [Size; 24] = [
    sz(10, 3, 5, 1, 1),
    sz(12, 5, 7, 1, 1),
    sz(14, 8, 10, 1, 1),
    sz(16, 12, 12, 1, 1),
    sz(18, 18, 14, 1, 1),
    sz(20, 22, 18, 1, 1),
    sz(22, 30, 20, 1, 1),
    sz(24, 36, 24, 1, 1),
    sz(26, 44, 28, 1, 1),
    sz(32, 62, 36, 2, 1),
    sz(36, 86, 42, 2, 1),
    sz(40, 114, 48, 2, 1),
    sz(44, 144, 56, 2, 1),
    sz(48, 174, 68, 2, 1),
    sz(52, 204, 84, 2, 2),
    sz(64, 280, 112, 4, 2),
    sz(72, 368, 144, 4, 4),
    sz(80, 456, 192, 4, 4),
    sz(88, 576, 224, 4, 4),
    sz(96, 696, 272, 4, 4),
    sz(104, 816, 336, 4, 6),
    sz(120, 1050, 408, 6, 6),
    sz(132, 1304, 496, 6, 8),
    sz(144, 1558, 620, 6, 10),
];

const FNC1: u8 = 232;
const UPPER_SHIFT: u8 = 235;
const ECI: u8 = 241;
const PAD: u8 = 129;

/// The data codewords of plain text: Latin-1 bytes when every character fits (the default
/// interpretation of Data Matrix), otherwise ECI 26 (UTF-8) followed by the UTF-8 bytes.
pub(crate) fn text_units(data: &str) -> Vec<Unit> {
    if data.chars().all(|c| (c as u32) < 256) {
        data.chars().map(|c| Unit::Byte(c as u32 as u8)).collect()
    } else {
        data.bytes().map(Unit::Byte).collect()
    }
}

/// ASCII encodation of units.
fn codewords(units: &[Unit], utf8: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if utf8 {
        // ECI 26 is written as the single codeword 26 + 1.
        out.extend([ECI, 27]);
    }
    let mut i = 0;
    while i < units.len() {
        match units[i] {
            Unit::Fnc1 => out.push(FNC1),
            Unit::Byte(a) => {
                if let Some(Unit::Byte(b)) = units.get(i + 1) {
                    if a.is_ascii_digit() && b.is_ascii_digit() {
                        out.push(130 + (a - b'0') * 10 + (b - b'0'));
                        i += 2;
                        continue;
                    }
                }
                if a < 128 {
                    out.push(a + 1);
                } else {
                    out.extend([UPPER_SHIFT, a - 128 + 1]);
                }
            }
        }
        i += 1;
    }
    out
}

/// GF(256) with the prime polynomial x^8 + x^5 + x^3 + x^2 + 1 (301).
struct Gf {
    exp: [u8; 512],
    log: [u8; 256],
}

impl Gf {
    fn new() -> Gf {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u32 = 1;
        for (i, e) in exp.iter_mut().enumerate().take(255) {
            *e = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 301;
            }
        }
        for i in 255..512 {
            exp[i] = exp[i - 255];
        }
        Gf { exp, log }
    }

    fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
        }
    }
}

/// Reed-Solomon check codewords of one block (generator roots alpha^1 .. alpha^n).
fn reed_solomon(gf: &Gf, data: &[u8], n: usize) -> Vec<u8> {
    // The generator, highest degree first, leading 1 dropped at the end.
    let mut g = vec![1u8];
    for i in 1..=n {
        let root = gf.exp[i];
        let mut next = vec![0u8; g.len() + 1];
        for (j, c) in g.iter().enumerate() {
            next[j] ^= *c;
            next[j + 1] ^= gf.mul(*c, root);
        }
        g = next;
    }
    let mut ecc = vec![0u8; n];
    for d in data {
        let m = d ^ ecc[0];
        ecc.remove(0);
        ecc.push(0);
        for (e, c) in ecc.iter_mut().zip(&g[1..]) {
            *e ^= gf.mul(m, *c);
        }
    }
    ecc
}

/// Module placement of ISO/IEC 16022 annex F: for each module of the mapping matrix, the
/// codeword (index) and the bit (0 = most significant), or `None` for the fixed corner.
struct Placement {
    nrow: usize,
    ncol: usize,
    cells: Vec<Option<(usize, u8)>>,
    filled: Vec<bool>,
}

impl Placement {
    fn module(&mut self, mut row: isize, mut col: isize, chr: usize, bit: u8) {
        let (nrow, ncol) = (self.nrow as isize, self.ncol as isize);
        if row < 0 {
            row += nrow;
            col += 4 - ((nrow + 4) % 8);
        }
        if col < 0 {
            col += ncol;
            row += 4 - ((ncol + 4) % 8);
        }
        let k = row as usize * self.ncol + col as usize;
        self.cells[k] = Some((chr, bit));
        self.filled[k] = true;
    }

    fn utah(&mut self, row: isize, col: isize, chr: usize) {
        self.module(row - 2, col - 2, chr, 0);
        self.module(row - 2, col - 1, chr, 1);
        self.module(row - 1, col - 2, chr, 2);
        self.module(row - 1, col - 1, chr, 3);
        self.module(row - 1, col, chr, 4);
        self.module(row, col - 2, chr, 5);
        self.module(row, col - 1, chr, 6);
        self.module(row, col, chr, 7);
    }

    fn corner(&mut self, which: u8, chr: usize) {
        let (r, c) = (self.nrow as isize, self.ncol as isize);
        let cells: [(isize, isize); 8] = match which {
            1 => [
                (r - 1, 0),
                (r - 1, 1),
                (r - 1, 2),
                (0, c - 2),
                (0, c - 1),
                (1, c - 1),
                (2, c - 1),
                (3, c - 1),
            ],
            2 => [
                (r - 3, 0),
                (r - 2, 0),
                (r - 1, 0),
                (0, c - 4),
                (0, c - 3),
                (0, c - 2),
                (0, c - 1),
                (1, c - 1),
            ],
            3 => [
                (r - 3, 0),
                (r - 2, 0),
                (r - 1, 0),
                (0, c - 2),
                (0, c - 1),
                (1, c - 1),
                (2, c - 1),
                (3, c - 1),
            ],
            _ => [
                (r - 1, 0),
                (r - 1, c - 1),
                (0, c - 3),
                (0, c - 2),
                (0, c - 1),
                (1, c - 3),
                (1, c - 2),
                (1, c - 1),
            ],
        };
        for (bit, (row, col)) in cells.into_iter().enumerate() {
            self.module(row, col, chr, bit as u8);
        }
    }

    fn filled(&self, row: isize, col: isize) -> bool {
        self.filled[row as usize * self.ncol + col as usize]
    }

    fn new(nrow: usize, ncol: usize) -> Placement {
        let mut p = Placement {
            nrow,
            ncol,
            cells: vec![None; nrow * ncol],
            filled: vec![false; nrow * ncol],
        };
        let (nr, nc) = (nrow as isize, ncol as isize);
        let (mut row, mut col, mut chr) = (4isize, 0isize, 0usize);
        loop {
            if row == nr && col == 0 {
                p.corner(1, chr);
                chr += 1;
            }
            if row == nr - 2 && col == 0 && nc % 4 != 0 {
                p.corner(2, chr);
                chr += 1;
            }
            if row == nr - 2 && col == 0 && nc % 8 == 4 {
                p.corner(3, chr);
                chr += 1;
            }
            if row == nr + 4 && col == 2 && nc % 8 == 0 {
                p.corner(4, chr);
                chr += 1;
            }
            loop {
                if row < nr && col >= 0 && !p.filled(row, col) {
                    p.utah(row, col, chr);
                    chr += 1;
                }
                row -= 2;
                col += 2;
                if !(row >= 0 && col < nc) {
                    break;
                }
            }
            row += 1;
            col += 3;
            loop {
                if row >= 0 && col < nc && !p.filled(row, col) {
                    p.utah(row, col, chr);
                    chr += 1;
                }
                row += 2;
                col -= 2;
                if !(row < nr && col >= 0) {
                    break;
                }
            }
            row += 3;
            col += 1;
            if !(row < nr || col < nc) {
                break;
            }
        }
        // The fixed pattern in the lower right corner when the mapping leaves it empty.
        let last = nrow * ncol - 1;
        if !p.filled[last] {
            p.filled[last] = true;
            p.filled[last - ncol - 1] = true;
        }
        p
    }
}

/// A Data Matrix: modules per side and the dark modules, row-major.
pub(crate) struct Symbol {
    pub side: usize,
    pub dark: Vec<bool>,
}

/// Encodes units in the smallest square symbol that holds them.
pub(crate) fn encode(units: &[Unit], utf8: bool) -> Result<Symbol, String> {
    let mut cw = codewords(units, utf8);
    let size = SIZES.iter().find(|s| s.data >= cw.len()).ok_or_else(|| {
        format!(
            "the data is too long for a Data Matrix: {} codewords, the largest symbol \
(144x144) holds 1558",
            cw.len()
        )
    })?;
    // Padding: the first pad is 129, the others are scrambled by the 253-state algorithm.
    if cw.len() < size.data {
        cw.push(PAD);
    }
    while cw.len() < size.data {
        let position = cw.len() + 1;
        let pseudo = (149 * position) % 253 + 1;
        let mut v = PAD as usize + pseudo;
        if v > 254 {
            v -= 254;
        }
        cw.push(v as u8);
    }
    // Interleaved blocks: block b takes the codewords b, b + blocks, b + 2 blocks, ...
    let gf = Gf::new();
    let ecc_per_block = size.ecc / size.blocks;
    let mut all = cw.clone();
    all.resize(size.data + size.ecc, 0);
    for b in 0..size.blocks {
        let block: Vec<u8> = cw.iter().skip(b).step_by(size.blocks).copied().collect();
        let ecc = reed_solomon(&gf, &block, ecc_per_block);
        for (k, e) in ecc.into_iter().enumerate() {
            all[size.data + b + k * size.blocks] = e;
        }
    }
    // Mapping matrix, then the data regions with their finder and timing patterns.
    let region = size.side / size.regions - 2;
    let n = region * size.regions;
    let placement = Placement::new(n, n);
    let side = size.side;
    let mut dark = vec![false; side * side];
    for r in 0..n {
        for c in 0..n {
            let k = r * n + c;
            let on = match placement.cells[k] {
                Some((chr, bit)) => all[chr] & (0x80 >> bit) != 0,
                None => placement.filled[k] && is_fixed_dark(n, r, c),
            };
            let y = r + 1 + 2 * (r / region);
            let x = c + 1 + 2 * (c / region);
            dark[y * side + x] = on;
        }
    }
    let step = region + 2;
    for ry in 0..size.regions {
        for rx in 0..size.regions {
            let (y0, x0) = (ry * step, rx * step);
            for i in 0..step {
                // Solid left and bottom edges.
                dark[(y0 + i) * side + x0] = true;
                dark[(y0 + step - 1) * side + x0 + i] = true;
                // Alternating top and right edges.
                dark[y0 * side + x0 + i] = i % 2 == 0;
                dark[(y0 + i) * side + x0 + step - 1] = i % 2 == 1;
            }
            dark[(y0 + step - 1) * side + x0] = true;
            dark[(y0 + step - 1) * side + x0 + step - 1] = true;
        }
    }
    Ok(Symbol { side, dark })
}

/// The fixed corner pattern: dark at the very corner and diagonally above-left of it.
fn is_fixed_dark(n: usize, r: usize, c: usize) -> bool {
    (r == n - 1 && c == n - 1) || (r == n - 2 && c == n - 2)
}
