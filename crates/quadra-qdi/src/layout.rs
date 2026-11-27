//! The grid: from rows and columns to the JSON's `layout` (`{x, y, w, h}` on 24 columns), and
//! back, by cutting a page into bands and columns.
//!
//! **Placing** (a file → the JSON). A page's rows are stacked from the top (`space N` leaves N
//! grid rows empty); a row's children are side by side from its left (`space N`: N empty
//! columns), a column's stacked from its top. A child without `width` in a row gets an equal
//! part of what is left (the first ones one more when it does not divide); a component without
//! `height` in a row is as high as the row; in a column, an equal part of the height left. A
//! row without `height` is as high as its highest child says. `at x, y size w x h` places a
//! component on the page, whatever the rows.
//!
//! **Cutting** (the JSON → a file). The page is cut into bands, where no component crosses
//! from one to the next; a band into the groups of components side by side; a group into
//! bands again, and so on. A component alone in a band (or in a group) is written as it is; a
//! band (a page's) that cannot be cut down to its components (they overlap, or turn around a
//! hole) is written with `at` for each of its components, followed by `space` its height.

use crate::ast::*;
use crate::Diag;

pub const COLUMNS: u32 = 24;
pub const MAX_HEIGHT: u32 = 200;

/// The components of a page, in the order they are written, with their place.
pub fn place_page<'a>(body: &'a [Block], diags: &mut Vec<Diag>) -> Vec<(&'a Component, [u32; 4])> {
    let mut out = Vec::new();
    let mut y = 0u32;
    for b in body {
        match b {
            Block::Space(n, _) => y += n,
            Block::Row(r) => y += place_row(r, 0, COLUMNS, y, &mut out, diags),
            Block::Component(c) => match c.at {
                Some([x, cy, w, h]) => {
                    if w == 0 || h == 0 || x + w > COLUMNS || h > MAX_HEIGHT {
                        diags.push(Diag::new(format!("`at {x}, {cy} size {w} x {h}`: 1 to 24 columns wide within the 24 columns, 1 to {MAX_HEIGHT} rows high"), c.span));
                    }
                    out.push((&**c, [x, cy, w, h]));
                }
                None => diags.push(Diag::new(
                    "a component of a page is in a `row`, or placed with `at`",
                    c.span,
                )),
            },
            Block::Column(c) => diags.push(Diag::new("a column is in a row", c.span)),
        }
    }
    out
}

/// The height a row's children ask for (none: they do not say).
fn natural_row(r: &Row) -> Option<u32> {
    r.height.or_else(|| {
        r.children
            .iter()
            .filter_map(|c| match c {
                Block::Component(c) => c.height,
                Block::Column(c) => natural_column(c),
                _ => None,
            })
            .max()
    })
}

fn natural_column(c: &Column) -> Option<u32> {
    let mut sum = 0;
    for ch in &c.children {
        sum += match ch {
            Block::Space(n, _) => *n,
            Block::Component(c) => c.height?,
            Block::Row(r) => natural_row(r)?,
            Block::Column(_) => return None,
        };
    }
    Some(sum)
}

/// Equal parts of `left` for the `n` children without a size; `None` when they would be 0.
fn shares(left: i64, n: usize) -> Option<Vec<u32>> {
    if n == 0 {
        return Some(Vec::new());
    }
    if left < n as i64 {
        return None;
    }
    let base = left as u32 / n as u32;
    let rem = left as u32 % n as u32;
    Some((0..n as u32).map(|k| base + u32::from(k < rem)).collect())
}

fn place_row<'a>(
    r: &'a Row,
    x0: u32,
    width: u32,
    y: u32,
    out: &mut Vec<(&'a Component, [u32; 4])>,
    diags: &mut Vec<Diag>,
) -> u32 {
    let Some(h) = natural_row(r) else {
        diags.push(Diag::new(
            "a row needs a `height` (or children with one)",
            r.span,
        ));
        return 0;
    };
    if h == 0 || h > MAX_HEIGHT {
        diags.push(Diag::new(
            format!("a row is 1 to {MAX_HEIGHT} grid rows high"),
            r.span,
        ));
    }
    let sizes: Vec<Option<u32>> = r
        .children
        .iter()
        .map(|c| match c {
            Block::Space(n, _) => Some(*n),
            Block::Component(c) => c.width,
            Block::Column(c) => c.width,
            Block::Row(_) => Some(0),
        })
        .collect();
    let fixed: i64 = sizes.iter().flatten().map(|n| *n as i64).sum();
    let unknown = sizes.iter().filter(|s| s.is_none()).count();
    let parts = match shares(width as i64 - fixed, unknown) {
        Some(p) if fixed <= width as i64 => p,
        _ => {
            diags.push(Diag::new(
                format!(
                    "the row is {width} columns wide: its parts take {fixed}{}",
                    if unknown > 0 {
                        format!(", and {unknown} more need at least one each")
                    } else {
                        String::new()
                    }
                ),
                r.span,
            ));
            // placed anyway, for the problems of what is in it
            vec![1; unknown]
        }
    };
    let mut parts = parts.into_iter();
    let mut x = x0;
    for (c, size) in r.children.iter().zip(sizes) {
        let w = size.unwrap_or_else(|| parts.next().unwrap());
        match c {
            Block::Space(..) => {}
            Block::Component(c) => {
                let ch = c.height.unwrap_or(h);
                if ch == 0 || ch > h {
                    diags.push(Diag::new(
                        format!("the component is {ch} high, in a row {h} high"),
                        c.span,
                    ));
                }
                if w == 0 {
                    diags.push(Diag::new("a component is at least 1 column wide", c.span));
                }
                out.push((&**c, [x, y, w, ch]));
            }
            Block::Column(col) => place_column(col, x, w, y, h, out, diags),
            Block::Row(r) => diags.push(Diag::new("a row is in a page or a column", r.span)),
        }
        x += w;
    }
    h
}

fn place_column<'a>(
    col: &'a Column,
    x: u32,
    width: u32,
    y0: u32,
    height: u32,
    out: &mut Vec<(&'a Component, [u32; 4])>,
    diags: &mut Vec<Diag>,
) {
    let sizes: Vec<Option<u32>> = col
        .children
        .iter()
        .map(|c| match c {
            Block::Space(n, _) => Some(*n),
            Block::Component(c) => c.height,
            Block::Row(r) => Some(natural_row(r).unwrap_or(0)),
            Block::Column(_) => Some(0),
        })
        .collect();
    let fixed: i64 = sizes.iter().flatten().map(|n| *n as i64).sum();
    let unknown = sizes.iter().filter(|s| s.is_none()).count();
    let parts = match shares(height as i64 - fixed, unknown) {
        Some(p) if fixed <= height as i64 => p,
        _ => {
            diags.push(Diag::new(
                format!(
                    "the column is {height} high: its parts take {fixed}{}",
                    if unknown > 0 {
                        format!(", and {unknown} more need at least one each")
                    } else {
                        String::new()
                    }
                ),
                col.span,
            ));
            return;
        }
    };
    let mut parts = parts.into_iter();
    let mut y = y0;
    for (c, size) in col.children.iter().zip(sizes) {
        let h = size.unwrap_or_else(|| parts.next().unwrap());
        match c {
            Block::Space(..) => y += h,
            Block::Component(c) => {
                if c.width.is_some_and(|w| w != width) {
                    diags.push(Diag::new(
                        format!("a component in a column is as wide as the column ({width})"),
                        c.span,
                    ));
                }
                out.push((&**c, [x, y, width, h]));
                y += h;
            }
            Block::Row(r) => {
                y += place_row(r, x, width, y, out, diags);
            }
            Block::Column(c) => diags.push(Diag::new("a column is in a row", c.span)),
        }
    }
}

// ---- cutting ---------------------------------------------------------------------------------------

/// A page cut into rows and columns; components by their index.
#[derive(Clone, Debug, PartialEq)]
pub enum Cut {
    Row {
        height: u32,
        children: Vec<Cut>,
    },
    Column {
        width: u32,
        children: Vec<Cut>,
    },
    Space(u32),
    /// in a row: its width, its height when lower than the row's; in a column: its height;
    /// on the page: `at`
    Comp {
        idx: usize,
        width: Option<u32>,
        height: Option<u32>,
        at: Option<[u32; 4]>,
    },
}

type Item = (usize, [u32; 4]);

/// Groups of items whose spans (`span(item) = (start, end)`) do not cross from one group to
/// the next, in order.
fn groups(
    mut items: Vec<Item>,
    span: impl Fn(&[u32; 4]) -> (u32, u32),
) -> Vec<(u32, u32, Vec<Item>)> {
    items.sort_by_key(|(i, r)| (span(r).0, r[1], r[0], *i));
    let mut out: Vec<(u32, u32, Vec<Item>)> = Vec::new();
    for it in items {
        let (s, e) = span(&it.1);
        match out.last_mut() {
            Some(g) if s < g.1 => {
                g.1 = g.1.max(e);
                g.2.push(it);
            }
            _ => out.push((s, e, vec![it])),
        }
    }
    for g in &mut out {
        g.2.sort_by_key(|(i, r)| (r[1], r[0], *i));
    }
    out
}

pub fn cut_page(items: &[Item]) -> Vec<Cut> {
    let mut out = Vec::new();
    let mut y = 0;
    for (top, bottom, band) in groups(items.to_vec(), |r| (r[1], r[1] + r[3])) {
        if top > y {
            out.push(Cut::Space(top - y));
        }
        match cut_row(&band, 0, COLUMNS, top, bottom - top) {
            Some(children) => out.push(Cut::Row {
                height: bottom - top,
                children,
            }),
            None => {
                for (idx, r) in &band {
                    out.push(Cut::Comp {
                        idx: *idx,
                        width: None,
                        height: None,
                        at: Some(*r),
                    });
                }
                out.push(Cut::Space(bottom - top));
            }
        }
        y = bottom;
    }
    out
}

fn cut_row(items: &[Item], x0: u32, _width: u32, top: u32, height: u32) -> Option<Vec<Cut>> {
    let mut out = Vec::new();
    let mut x = x0;
    for (left, right, g) in groups(items.to_vec(), |r| (r[0], r[0] + r[2])) {
        if left > x {
            out.push(Cut::Space(left - x));
        }
        if g.len() == 1 && g[0].1[1] == top {
            let (idx, r) = g[0];
            out.push(Cut::Comp {
                idx,
                width: Some(r[2]),
                height: (r[3] != height).then_some(r[3]),
                at: None,
            });
        } else {
            out.push(Cut::Column {
                width: right - left,
                children: cut_column(&g, left, right - left, top, height)?,
            });
        }
        x = right;
    }
    Some(out)
}

fn cut_column(items: &[Item], x0: u32, width: u32, top: u32, _height: u32) -> Option<Vec<Cut>> {
    let bands = groups(items.to_vec(), |r| (r[1], r[1] + r[3]));
    if bands.len() == 1 && items.len() > 1 {
        return None;
    }
    let mut out = Vec::new();
    let mut y = top;
    for (t, b, band) in bands {
        if t > y {
            out.push(Cut::Space(t - y));
        }
        if band.len() == 1 && band[0].1[0] == x0 && band[0].1[2] == width {
            let (idx, r) = band[0];
            out.push(Cut::Comp {
                idx,
                width: None,
                height: Some(r[3]),
                at: None,
            });
        } else {
            let children = cut_row(&band, x0, width, t, b - t)?;
            // a band of one group as wide as the column, that the row cannot split: no progress
            if children.len() == 1
                && matches!(children[0], Cut::Column { .. })
                && band.len() == items.len()
            {
                return None;
            }
            out.push(Cut::Row {
                height: b - t,
                children,
            });
        }
        y = b;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rects(
        cuts: &[Cut],
        x0: u32,
        y0: u32,
        w: u32,
        h: u32,
        row: bool,
        out: &mut Vec<(usize, [u32; 4])>,
    ) -> u32 {
        // the inverse of cutting, on the cut tree directly (the parser's placing is tested
        // through files)
        let mut x = x0;
        let mut y = y0;
        for c in cuts {
            match c {
                Cut::Space(n) => {
                    if row {
                        x += n
                    } else {
                        y += n
                    }
                }
                Cut::Comp {
                    idx,
                    width,
                    height,
                    at,
                } => {
                    if let Some(a) = at {
                        out.push((*idx, *a));
                    } else if row {
                        out.push((*idx, [x, y, width.unwrap(), height.unwrap_or(h)]));
                        x += width.unwrap();
                    } else {
                        out.push((*idx, [x, y, w, height.unwrap()]));
                        y += height.unwrap();
                    }
                }
                Cut::Row { height, children } => {
                    rects(children, x, y, w, *height, true, out);
                    y += height;
                }
                Cut::Column { width, children } => {
                    rects(children, x, y, *width, h, false, out);
                    x += width;
                }
            }
        }
        if row {
            h
        } else {
            y - y0
        }
    }

    fn roundtrip(items: Vec<Item>) -> Vec<Cut> {
        let cuts = cut_page(&items);
        let mut back = Vec::new();
        rects(&cuts, 0, 0, COLUMNS, 0, false, &mut back);
        let mut a = items.clone();
        a.sort();
        back.sort();
        assert_eq!(a, back, "{cuts:#?}");
        cuts
    }

    #[test]
    fn cockpit() {
        let cuts = roundtrip(vec![
            (0, [0, 0, 24, 2]),
            (1, [0, 2, 24, 2]),
            (2, [0, 4, 8, 3]),
            (3, [8, 4, 6, 3]),
            (4, [14, 4, 5, 3]),
            (5, [19, 4, 5, 3]),
            (6, [0, 7, 14, 7]),
            (7, [14, 7, 10, 7]),
            (8, [0, 14, 16, 9]),
            (9, [16, 14, 8, 2]),
            (10, [16, 16, 8, 2]),
            (11, [16, 18, 8, 5]),
            (12, [0, 23, 24, 8]),
        ]);
        assert_eq!(cuts.len(), 6);
        assert!(
            matches!(&cuts[4], Cut::Row { height: 9, children } if matches!(&children[1], Cut::Column { width: 8, .. }))
        );
    }

    #[test]
    fn holes_offsets_and_overlaps() {
        roundtrip(vec![
            (0, [2, 3, 4, 2]),
            (1, [10, 4, 4, 6]),
            (2, [0, 12, 6, 2]),
        ]);
        // a pinwheel cannot be cut: placed
        let cuts = roundtrip(vec![
            (0, [0, 0, 8, 2]),
            (1, [8, 0, 2, 8]),
            (2, [2, 8, 8, 2]),
            (3, [0, 2, 2, 8]),
            (4, [2, 2, 6, 6]),
        ]);
        assert!(cuts
            .iter()
            .any(|c| matches!(c, Cut::Comp { at: Some(_), .. })));
        roundtrip(vec![(0, [0, 0, 6, 4]), (1, [3, 2, 6, 4])]);
        // nested: a column holding a row of two
        roundtrip(vec![
            (0, [0, 0, 12, 6]),
            (1, [12, 0, 12, 2]),
            (2, [12, 2, 6, 4]),
            (3, [18, 2, 6, 4]),
        ]);
    }
}
