//! Quadra's documents. A `document` of a `.qdp` file is evaluated by the runtime into a
//! [`Doc`], a tree of blocks (headings, paragraphs, tables, images…) with every value already
//! turned into text; this crate writes it as Typst markup and compiles it to PDF with the
//! Typst compiler embedded as a library (no browser, no external program), with the fonts
//! in the binary. The same tree always gives the same bytes.

pub mod typst;

pub use crate::typst::Pdf;
