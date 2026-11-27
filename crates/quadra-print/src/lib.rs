//! Quadra's documents. A `document` of a `.qdp` file is evaluated by the runtime into a
//! [`Doc`], a tree of blocks (headings, paragraphs, tables, images…) with every value already
//! turned into text; this crate writes it as Typst markup and compiles it to PDF with the
//! Typst compiler embedded as a library (no browser, no external program), with the fonts
//! in the binary. The same tree always gives the same bytes.

pub mod doc;
pub mod typst;

pub use crate::doc::{Align, Block, Column, Doc, Style};
pub use crate::typst::Pdf;

/// Render a document: its Typst source compiled with the files it uses (images, barcodes).
pub fn render(doc: &Doc, files: &[(String, Vec<u8>)], ident: &str) -> Result<Pdf, String> {
    typst::pdf(&doc::to_typst(doc), files, ident)
}
