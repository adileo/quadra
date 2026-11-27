//! PDF: one label per page, or labels in a grid on sheets.

use std::collections::{BTreeSet, HashMap};

use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect, Ref, Str};

use crate::font::{self, LINE_SPACING, PT_PER_MM};
use crate::image::Rgba;
use crate::layout::{self, fmt_mm, Item, HR_PT};
use crate::svg::{runs, HR_GAP_MM};
use crate::{Align, Label, Sheet};

/// Where a font's baseline is below the top of its line, as a multiple of the size.
const BASELINE: f64 = 0.85;

/// FNV-1a, 128 bits: the document identifier is a hash of what the document shows, so the
/// same labels always give the same identifier (PDF readers use it to tell files apart).
struct Hash(u128);

impl Hash {
    fn new() -> Hash {
        Hash(0x6c62272e07bb014262b821756295c58d)
    }
    fn update(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u128;
            self.0 = self.0.wrapping_mul(0x0000000001000000000000000000013B);
        }
    }
}

/// A page: width and height in mm, and the labels on it (index, x and y offsets in mm).
type Page = (f64, f64, Vec<(usize, f64, f64)>);

fn deflate(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

/// A page being drawn: its height in points and where the current label's corner is.
struct Pen {
    page_h: f64,
    ox: f64,
    oy: f64,
}

impl Pen {
    fn x(&self, mm: f64) -> f32 {
        ((self.ox + mm) * PT_PER_MM) as f32
    }
    fn y(&self, mm: f64) -> f32 {
        (self.page_h - (self.oy + mm) * PT_PER_MM) as f32
    }
    /// A rectangle given by its top-left corner and size in millimetres.
    fn rect(&self, c: &mut Content, x: f64, y: f64, w: f64, h: f64) {
        c.rect(
            self.x(x),
            self.y(y + h),
            (w * PT_PER_MM) as f32,
            (h * PT_PER_MM) as f32,
        );
    }
}

fn text_line(c: &mut Content, pen: &Pen, x: f64, baseline: f64, size_pt: f64, bold: bool, s: &str) {
    let bytes = font::encode(s).unwrap_or_default();
    c.begin_text();
    c.set_font(Name(if bold { b"F2" } else { b"F1" }), size_pt as f32);
    c.next_line(pen.x(x), pen.y(baseline));
    c.show(Str(&bytes));
    c.end_text();
}

/// The labels as a PDF.
///
/// Without a sheet each label is a page of its own size. With a [`Sheet`] the labels fill
/// its grid, left to right and top to bottom, as many pages as needed.
///
/// Barcodes are filled rectangles from [`crate::encode`] (exact module widths, no
/// rounding to a printer's dots); text uses Helvetica and Helvetica-Bold, two of the 14
/// standard PDF fonts, which are not embedded; images are embedded (RGB, with a soft mask
/// when they have transparency). The output has no dates and no random identifiers: the
/// file identifier is a hash of the pages, so the same labels give the same bytes.
pub fn to_pdf(labels: &[Label], sheet: Option<&Sheet>) -> Result<Vec<u8>, String> {
    if labels.is_empty() {
        return Err("there are no labels".into());
    }
    let mut laid = Vec::with_capacity(labels.len());
    for (i, label) in labels.iter().enumerate() {
        laid.push(layout::layout(i + 1, label)?);
    }
    // Pages: width and height in mm, and the labels on them with their offsets.
    let mut pages: Vec<Page> = Vec::new();
    match sheet {
        None => {
            for (i, l) in labels.iter().enumerate() {
                pages.push((l.width_mm, l.height_mm, vec![(i, 0.0, 0.0)]));
            }
        }
        Some(s) => {
            check_sheet(s, labels)?;
            let per = (s.cols * s.rows) as usize;
            let (w, h) = (labels[0].width_mm, labels[0].height_mm);
            for (i, _) in labels.iter().enumerate() {
                let cell = i % per;
                if cell == 0 {
                    pages.push((s.page_w_mm, s.page_h_mm, Vec::new()));
                }
                let col = (cell % s.cols as usize) as f64;
                let row = (cell / s.cols as usize) as f64;
                let ox = s.margin_left_mm + col * (w + s.gap_x_mm);
                let oy = s.margin_top_mm + row * (h + s.gap_y_mm);
                pages.last_mut().expect("a page").2.push((i, ox, oy));
            }
        }
    }

    // Objects: 1 catalog, 2 page tree, 3 and 4 fonts, then a page and its content for each
    // page, then each image (and its soft mask) in order of first use.
    let (catalog, tree, f1, f2) = (Ref::new(1), Ref::new(2), Ref::new(3), Ref::new(4));
    let first_image = 5 + 2 * pages.len() as i32;
    let mut numbers: HashMap<&[u8], usize> = HashMap::new();
    let mut images: Vec<&Rgba> = Vec::new();
    for (_, _, on_page) in &pages {
        for (li, _, _) in on_page {
            for item in &laid[*li] {
                if let Item::Image { png, image, .. } = item {
                    numbers.entry(png.as_slice()).or_insert_with(|| {
                        images.push(image);
                        images.len() - 1
                    });
                }
            }
        }
    }

    let mut pdf = Pdf::new();
    let mut hash = Hash::new();
    let mut page_refs = Vec::new();
    for (k, (pw, ph, on_page)) in pages.iter().enumerate() {
        let page_id = Ref::new(5 + 2 * k as i32);
        let content_id = Ref::new(6 + 2 * k as i32);
        page_refs.push(page_id);
        let page_h = ph * PT_PER_MM;
        let mut c = Content::new();
        c.set_fill_gray(0.0);
        c.set_stroke_gray(0.0);
        let mut used = BTreeSet::new();
        for (li, ox, oy) in on_page {
            let pen = Pen {
                page_h,
                ox: *ox,
                oy: *oy,
            };
            for item in &laid[*li] {
                let mut n = 0;
                if let Item::Image { png, .. } = item {
                    n = numbers[png.as_slice()];
                    used.insert(n);
                }
                draw(&mut c, &pen, item, n);
            }
        }
        let raw = c.finish().into_vec();
        hash.update(format!("page {pw} {ph}\n").as_bytes());
        hash.update(&raw);
        pdf.stream(content_id, &deflate(&raw))
            .filter(Filter::FlateDecode);
        let mut page = pdf.page(page_id);
        page.media_box(Rect::new(0.0, 0.0, (pw * PT_PER_MM) as f32, page_h as f32));
        page.parent(tree);
        page.contents(content_id);
        let mut res = page.resources();
        res.fonts().pair(Name(b"F1"), f1).pair(Name(b"F2"), f2);
        if !used.is_empty() {
            let mut xo = res.x_objects();
            for n in &used {
                xo.pair(
                    Name(format!("Im{n}").as_bytes()),
                    Ref::new(first_image + 2 * *n as i32),
                );
            }
            xo.finish();
        }
        res.finish();
        page.finish();
    }

    for (n, img) in images.iter().enumerate() {
        hash.update(format!("image {} {}\n", img.width, img.height).as_bytes());
        hash.update(&img.pixels);
        let id = Ref::new(first_image + 2 * n as i32);
        let mask_id = Ref::new(first_image + 2 * n as i32 + 1);
        let mut rgb = Vec::with_capacity(img.width * img.height * 3);
        let mut alpha = Vec::with_capacity(img.width * img.height);
        for p in img.pixels.chunks_exact(4) {
            rgb.extend_from_slice(&p[..3]);
            alpha.push(p[3]);
        }
        let has_alpha = img.has_alpha();
        let data = deflate(&rgb);
        let mut x = pdf.image_xobject(id, &data);
        x.filter(Filter::FlateDecode);
        x.width(img.width as i32);
        x.height(img.height as i32);
        x.color_space().device_rgb();
        x.bits_per_component(8);
        if has_alpha {
            x.s_mask(mask_id);
        }
        x.finish();
        if has_alpha {
            let mdata = deflate(&alpha);
            let mut m = pdf.image_xobject(mask_id, &mdata);
            m.filter(Filter::FlateDecode);
            m.width(img.width as i32);
            m.height(img.height as i32);
            m.color_space().device_gray();
            m.bits_per_component(8);
            m.finish();
        }
    }

    pdf.catalog(catalog).pages(tree);
    pdf.pages(tree)
        .kids(page_refs.iter().copied())
        .count(page_refs.len() as i32);
    for (id, name) in [(f1, &b"Helvetica"[..]), (f2, &b"Helvetica-Bold"[..])] {
        pdf.type1_font(id)
            .base_font(Name(name))
            .encoding_predefined(Name(b"WinAnsiEncoding"));
    }
    let id = hash.0.to_be_bytes().to_vec();
    pdf.set_file_id((id.clone(), id));
    Ok(pdf.finish())
}

fn check_sheet(s: &Sheet, labels: &[Label]) -> Result<(), String> {
    for (name, v) in [("page width", s.page_w_mm), ("page height", s.page_h_mm)] {
        if !(v.is_finite() && v > 0.0) {
            return Err(format!("the sheet's {name} must be more than 0, not {v}"));
        }
    }
    for (name, v) in [
        ("left margin", s.margin_left_mm),
        ("top margin", s.margin_top_mm),
        ("gap between columns", s.gap_x_mm),
        ("gap between rows", s.gap_y_mm),
    ] {
        if !(v.is_finite() && v >= 0.0) {
            return Err(format!("the sheet's {name} must be 0 or more, not {v}"));
        }
    }
    if s.cols == 0 || s.rows == 0 {
        return Err(format!(
            "the sheet must have at least one column and one row, not {} × {}",
            s.cols, s.rows
        ));
    }
    let (w, h) = (labels[0].width_mm, labels[0].height_mm);
    for (i, l) in labels.iter().enumerate().skip(1) {
        if (l.width_mm - w).abs() > 1e-6 || (l.height_mm - h).abs() > 1e-6 {
            return Err(format!(
                "label {} is {} × {} mm but the labels on the sheet are {} × {} mm (label 1): \
all labels on a sheet have the same size",
                i + 1,
                fmt_mm(l.width_mm),
                fmt_mm(l.height_mm),
                fmt_mm(w),
                fmt_mm(h)
            ));
        }
    }
    let right = s.margin_left_mm + s.cols as f64 * w + (s.cols - 1) as f64 * s.gap_x_mm;
    if right > s.page_w_mm + 1e-6 {
        return Err(format!(
            "{} columns of {} mm labels end at {} mm, past the page width of {} mm",
            s.cols,
            fmt_mm(w),
            fmt_mm(right),
            fmt_mm(s.page_w_mm)
        ));
    }
    let bottom = s.margin_top_mm + s.rows as f64 * h + (s.rows - 1) as f64 * s.gap_y_mm;
    if bottom > s.page_h_mm + 1e-6 {
        return Err(format!(
            "{} rows of {} mm labels end at {} mm, past the page height of {} mm",
            s.rows,
            fmt_mm(h),
            fmt_mm(bottom),
            fmt_mm(s.page_h_mm)
        ));
    }
    Ok(())
}

/// Draws an item; `image` is the resource number of the item's image when it is one.
fn draw(c: &mut Content, pen: &Pen, item: &Item, image: usize) {
    match item {
        Item::Text {
            x,
            y,
            size_pt,
            bold,
            box_w,
            align,
            lines,
        } => {
            let line_h = size_pt * LINE_SPACING / PT_PER_MM;
            for (i, line) in lines.iter().enumerate() {
                if line.is_empty() {
                    continue;
                }
                let w = font::width_mm(&font::encode(line).unwrap_or_default(), *size_pt, *bold);
                let left = match align {
                    Align::Left => *x,
                    Align::Center => x + (box_w - w) / 2.0,
                    Align::Right => x + box_w - w,
                };
                let baseline = y + i as f64 * line_h + BASELINE * size_pt / PT_PER_MM;
                text_line(c, pen, left, baseline, *size_pt, *bold, line);
            }
        }
        Item::Bars {
            x,
            y,
            module,
            height,
            bars,
            text,
            ..
        } => {
            for (start, len) in runs(bars) {
                pen.rect(
                    c,
                    x + start as f64 * module,
                    *y,
                    len as f64 * module,
                    *height,
                );
            }
            c.fill_nonzero();
            if let Some((shown, left, bw)) = text {
                let w = font::width_mm(&font::encode(shown).unwrap_or_default(), HR_PT, false);
                let baseline = y + height + HR_GAP_MM + 0.8 * HR_PT / PT_PER_MM;
                text_line(c, pen, left + (bw - w) / 2.0, baseline, HR_PT, false, shown);
            }
        }
        Item::Matrix {
            x,
            y,
            module,
            side,
            dark,
            ..
        } => {
            for row in 0..*side {
                for (start, len) in runs(&dark[row * side..(row + 1) * side]) {
                    pen.rect(
                        c,
                        x + start as f64 * module,
                        y + row as f64 * module,
                        len as f64 * module,
                        *module,
                    );
                }
            }
            c.fill_nonzero();
        }
        Item::Box { x, y, w, h, t } => {
            pen.rect(c, *x, *y, *w, *h);
            if w - 2.0 * t > 1e-9 && h - 2.0 * t > 1e-9 {
                pen.rect(c, x + t, y + t, w - 2.0 * t, h - 2.0 * t);
            }
            c.fill_even_odd();
        }
        Item::Line { x1, y1, x2, y2, t } => {
            c.save_state();
            c.set_line_width((t * PT_PER_MM) as f32);
            c.move_to(pen.x(*x1), pen.y(*y1));
            c.line_to(pen.x(*x2), pen.y(*y2));
            c.stroke();
            c.restore_state();
        }
        Item::Image { x, y, w, h, .. } => {
            let n = image;
            c.save_state();
            c.transform([
                (w * PT_PER_MM) as f32,
                0.0,
                0.0,
                (h * PT_PER_MM) as f32,
                pen.x(*x),
                pen.y(y + h),
            ]);
            c.x_object(Name(format!("Im{n}").as_bytes()));
            c.restore_state();
        }
    }
}
