//! Barcodes and labels: Code 128, GS1-128, SSCC, EAN-13, QR and Data Matrix symbols, and
//! labels of text, barcodes, boxes, lines and images rendered to SVG, ZPL (Zebra printers)
//! and PDF (laser printers, sheets of labels).
//!
//! Everything here is pure and deterministic: no files, no network, no clock, no random
//! numbers. The same input always gives the same bytes, so outputs can be compared, cached
//! and tested byte for byte.
//!
//! ```
//! use quadra_label::{encode, Modules, Symbology};
//!
//! let Modules::Linear { bars, text } = encode(Symbology::Ean13, "400638133393").unwrap() else {
//!     unreachable!()
//! };
//! assert_eq!(bars.len(), 95);
//! assert_eq!(text, "4006381333931");
//! ```
//!
//! - [`encode`] gives the modules of a symbol (bars, or a matrix of dark and light
//!   squares); [`svg`] draws one symbol as SVG, for documents.
//! - A [`Label`] is a size, a printer resolution and [`Element`]s placed in millimetres
//!   from the top-left corner; [`to_zpl`] gives the ZPL for Zebra printers and [`to_pdf`]
//!   a PDF, one label per page or arranged on a [`Sheet`].
//!
//! Errors are plain English sentences that say what is wrong and where.

mod code128;
mod datamatrix;
mod ean;
mod font;
mod gs1;
mod image;
mod layout;
mod pdf;
mod qr;
mod svg;
mod zpl;

pub use gs1::check_digit;
pub use pdf::to_pdf;
pub use svg::{svg, SvgOptions};
pub use zpl::to_zpl;

use gs1::Unit;

/// A kind of barcode.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Symbology {
    /// Code 128: any ASCII text. Code sets A, B and C are mixed to make the symbol as short
    /// as possible.
    Code128,
    /// EAN-13: 12 digits (the check digit is computed) or 13 (the check digit is verified).
    Ean13,
    /// GS1-128: GS1 element strings in human form, `(01)09501101530003(17)261231(10)LOT42`.
    /// The symbol starts with FNC1, and FNC1 separates a variable-length element string from
    /// the next one. The application identifiers 00, 01, 02, 10, 11, 13, 15, 17, 20, 21,
    /// 30, 37, 3100-3699, 400-403, 410-415, 420 and 421 are checked (lengths, digits,
    /// dates, check digits); others are refused.
    Gs1_128,
    /// SSCC (serial shipping container code) as GS1-128 with application identifier (00):
    /// 17 digits (extension digit, GS1 company prefix, serial reference; the check digit is
    /// computed) or 18 (the check digit is verified).
    Sscc,
    /// QR Code model 2 at an error correction level. Numeric, alphanumeric and byte modes
    /// are chosen automatically for the smallest symbol; text is UTF-8.
    Qr(QrEcc),
    /// Data Matrix ECC 200, square. Data in GS1 human form (starting with `(`) gives a GS1
    /// Data Matrix (FNC1 first, FNC1 after variable-length element strings); other text is
    /// stored as ISO-8859-1 when it can be, otherwise as UTF-8 with ECI 26.
    DataMatrix,
}

/// The error correction level of a QR code: how much of the symbol can be damaged and still
/// be read (about 7%, 15%, 25% and 30%). M is the usual choice and the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum QrEcc {
    L,
    #[default]
    M,
    Q,
    H,
}

impl Symbology {
    /// QR Code at error correction M.
    pub const QR: Symbology = Symbology::Qr(QrEcc::M);

    /// The symbology of a name, ignoring case, spaces, `-` and `_`: `code128`, `ean13`,
    /// `gs1-128`, `sscc`, `qr` (also `qr-l`, `qr-m`, `qr-q`, `qr-h`), `datamatrix`.
    pub fn from_name(name: &str) -> Option<Symbology> {
        let n: String = name
            .chars()
            .filter(|c| !matches!(c, ' ' | '-' | '_'))
            .flat_map(char::to_lowercase)
            .collect();
        Some(match n.as_str() {
            "code128" => Symbology::Code128,
            "ean13" => Symbology::Ean13,
            "gs1128" => Symbology::Gs1_128,
            "sscc" => Symbology::Sscc,
            "qr" | "qrcode" | "qrm" => Symbology::QR,
            "qrl" => Symbology::Qr(QrEcc::L),
            "qrq" => Symbology::Qr(QrEcc::Q),
            "qrh" => Symbology::Qr(QrEcc::H),
            "datamatrix" => Symbology::DataMatrix,
            _ => return None,
        })
    }

    /// The name used in messages: "Code 128", "EAN-13", "GS1-128", "SSCC", "QR",
    /// "Data Matrix".
    pub fn name(self) -> &'static str {
        match self {
            Symbology::Code128 => "Code 128",
            Symbology::Ean13 => "EAN-13",
            Symbology::Gs1_128 => "GS1-128",
            Symbology::Sscc => "SSCC",
            Symbology::Qr(_) => "QR",
            Symbology::DataMatrix => "Data Matrix",
        }
    }

    /// Whether the symbol is a matrix (QR, Data Matrix) rather than bars.
    pub fn is_matrix(self) -> bool {
        matches!(self, Symbology::Qr(_) | Symbology::DataMatrix)
    }

    /// The quiet zone the standard asks for, in modules, on each side: 10 for Code 128 and
    /// GS1-128, 11 for EAN-13 (its left side; 7 are enough on the right), 4 for QR, 1 for
    /// Data Matrix.
    pub fn quiet_zone(self) -> u32 {
        match self {
            Symbology::Code128 | Symbology::Gs1_128 | Symbology::Sscc => 10,
            Symbology::Ean13 => 11,
            Symbology::Qr(_) => 4,
            Symbology::DataMatrix => 1,
        }
    }

    /// The module width used when a barcode element does not give one, in millimetres:
    /// 0.5 for GS1-128 and SSCC (the GS1 logistic label range starts at 0.495), 0.33 for
    /// Code 128 and EAN-13 (EAN-13 at 100% size). Matrix symbols fill their height instead.
    pub fn default_module_mm(self) -> f64 {
        match self {
            Symbology::Gs1_128 | Symbology::Sscc => 0.5,
            _ => 0.33,
        }
    }
}

/// The modules of a symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Modules {
    /// Bars, left to right, one entry per module (`true` = dark), without quiet zones, and
    /// the human-readable line printed under them.
    Linear { bars: Vec<bool>, text: String },
    /// A matrix of modules, row-major from the top-left (`true` = dark), without quiet zone.
    Matrix {
        size_w: usize,
        size_h: usize,
        dark: Vec<bool>,
    },
}

impl Modules {
    /// The width in modules.
    pub fn width(&self) -> usize {
        match self {
            Modules::Linear { bars, .. } => bars.len(),
            Modules::Matrix { size_w, .. } => *size_w,
        }
    }
}

/// The modules of a symbol for `data`.
///
/// ```
/// use quadra_label::{encode, Symbology};
///
/// assert_eq!(
///     encode(Symbology::Ean13, "12345678901").unwrap_err(),
///     "EAN-13 takes 12 or 13 digits, not 11"
/// );
/// assert_eq!(
///     encode(Symbology::Ean13, "4006381333932").unwrap_err(),
///     "the check digit of `4006381333932` should be 1"
/// );
/// ```
pub fn encode(kind: Symbology, data: &str) -> Result<Modules, String> {
    match kind {
        Symbology::Code128 | Symbology::Gs1_128 | Symbology::Sscc => {
            let (units, text) = code128_units(kind, data)?;
            let tokens = code128::plan(&units)?;
            Ok(Modules::Linear {
                bars: code128::bars(&code128::values(&tokens)),
                text,
            })
        }
        Symbology::Ean13 => {
            let digits = ean::digits(data)?;
            Ok(Modules::Linear {
                bars: ean::bars(&digits),
                text: digits,
            })
        }
        Symbology::Qr(ecc) => {
            let (side, dark) = qr::encode(data, ecc)?;
            Ok(Modules::Matrix {
                size_w: side,
                size_h: side,
                dark,
            })
        }
        Symbology::DataMatrix => {
            let (units, utf8) = datamatrix_units(data)?;
            let symbol = datamatrix::encode(&units, utf8)?;
            Ok(Modules::Matrix {
                size_w: symbol.side,
                size_h: symbol.side,
                dark: symbol.dark,
            })
        }
    }
}

/// The units of a Code 128 family symbol and its human-readable line.
pub(crate) fn code128_units(kind: Symbology, data: &str) -> Result<(Vec<Unit>, String), String> {
    match kind {
        Symbology::Gs1_128 | Symbology::Sscc => {
            let elements = if kind == Symbology::Sscc {
                gs1::sscc(data)?
            } else {
                gs1::parse(data)?
            };
            let units = gs1::units(&elements);
            // GS1: at most 48 data characters in one GS1-128 symbol, separators included.
            let count = units.len() - 1;
            if count > 48 {
                return Err(format!(
                    "GS1-128 holds at most 48 characters (application identifiers and \
separators included), not {count}: split the data over two symbols"
                ));
            }
            Ok((units, gs1::human(&elements)))
        }
        _ => {
            if data.is_empty() {
                return Err("Code 128 needs at least one character".into());
            }
            Ok((code128::text_units(data)?, data.to_string()))
        }
    }
}

/// The units of a Data Matrix and whether they are UTF-8 (ECI 26).
pub(crate) fn datamatrix_units(data: &str) -> Result<(Vec<Unit>, bool), String> {
    if data.is_empty() {
        return Err("a Data Matrix needs at least one character".into());
    }
    if data.starts_with('(') {
        let elements = gs1::parse(data)?;
        return Ok((gs1::units(&elements), false));
    }
    let utf8 = data.chars().any(|c| c as u32 >= 256);
    Ok((datamatrix::text_units(data), utf8))
}

/// Whether Data Matrix data is GS1 element strings in human form.
pub(crate) fn is_gs1_datamatrix(data: &str) -> bool {
    data.starts_with('(')
}

/// A label: its size, the resolution of the printer it is meant for, and what is on it.
///
/// Coordinates are millimetres from the top-left corner of the label, x to the right and y
/// down.
#[derive(Clone, Debug, PartialEq)]
pub struct Label {
    pub width_mm: f64,
    pub height_mm: f64,
    /// Dots per inch of the thermal printer: 152, 203, 300 or 600. ZPL positions and sizes
    /// are converted to dots at this resolution; the PDF does not depend on it.
    pub dpi: u32,
    pub elements: Vec<Element>,
}

/// Horizontal alignment of text in its box.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// Something printed on a label. Positions are the top-left corner, in millimetres.
#[derive(Clone, Debug, PartialEq)]
pub enum Element {
    /// Text in a box that starts at `x_mm` and is `width_mm` wide (to the right edge of the
    /// label when `None`). Words wrap to the width; `\n` starts a new line. Lines are
    /// `size_pt` × 1.15 apart. The PDF uses Helvetica (Helvetica-Bold when `bold`), so the
    /// text must be in the Windows-1252 character set (Western European languages); ZPL uses
    /// the printer's font 0.
    Text {
        x_mm: f64,
        y_mm: f64,
        size_pt: f64,
        bold: bool,
        width_mm: Option<f64>,
        align: Align,
        text: String,
    },
    /// A barcode whose first bar (or top-left module) is at `x_mm`, `y_mm`. Bars are
    /// `height_mm` tall, `module_mm` wide (default [`Symbology::default_module_mm`]), with
    /// the human-readable line under them when `human_readable`. A matrix symbol is a square
    /// `height_mm` on each side, or modules of `module_mm` when given (it must then fit in
    /// `height_mm`); it has no human-readable line. The quiet zone
    /// ([`Symbology::quiet_zone`]) around the symbol must be inside the label too.
    Barcode {
        x_mm: f64,
        y_mm: f64,
        kind: Symbology,
        data: String,
        height_mm: f64,
        module_mm: Option<f64>,
        human_readable: bool,
    },
    /// A rectangle outline; the line is `thickness_mm` thick, inside the rectangle.
    Box {
        x_mm: f64,
        y_mm: f64,
        w_mm: f64,
        h_mm: f64,
        thickness_mm: f64,
    },
    /// A straight line between two points, `thickness_mm` thick, centred on the line.
    Line {
        x1_mm: f64,
        y1_mm: f64,
        x2_mm: f64,
        y2_mm: f64,
        thickness_mm: f64,
    },
    /// A PNG image scaled to `w_mm` × `h_mm`. The PDF embeds it in colour; ZPL prints it in
    /// black and white (a pixel is black when its luminance over a white background is
    /// below 50%).
    Image {
        x_mm: f64,
        y_mm: f64,
        w_mm: f64,
        h_mm: f64,
        png: Vec<u8>,
    },
}

/// A page of labels in a grid, for laser printers: for example an A4 sheet of 3 × 8 labels
/// of 70 × 37 mm is `Sheet { page_w_mm: 210.0, page_h_mm: 297.0, cols: 3, rows: 8,
/// margin_left_mm: 0.0, margin_top_mm: 0.5, gap_x_mm: 0.0, gap_y_mm: 0.0 }`.
///
/// Labels fill the grid left to right, then top to bottom, then the next page. All labels
/// on a sheet must have the same size, and the grid must fit the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sheet {
    pub page_w_mm: f64,
    pub page_h_mm: f64,
    pub cols: u32,
    pub rows: u32,
    /// Distance from the left edge of the page to the first column.
    pub margin_left_mm: f64,
    /// Distance from the top edge of the page to the first row.
    pub margin_top_mm: f64,
    /// Space between columns.
    pub gap_x_mm: f64,
    /// Space between rows.
    pub gap_y_mm: f64,
}
