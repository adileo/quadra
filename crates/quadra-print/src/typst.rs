//! The embedded Typst: a world with the main source, the files it may read (images of the
//! project) and the fonts compiled into the binary. Nothing comes from the disk, the network
//! or the clock, so the same source gives the same PDF.

use std::collections::HashMap;
use std::sync::OnceLock;

use typst::diag::{FileError, FileResult, SourceDiagnostic};
use typst::foundations::{Bytes, Datetime, Duration};
use typst::layout::{Frame, FrameItem};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_layout::PagedDocument;

/// The fonts in the binary: Libertinus Serif, New Computer Modern, DejaVu Sans Mono (the ones
/// of typst-assets) and those of the crate.
fn fonts() -> &'static (LazyHash<FontBook>, Vec<Font>) {
    static FONTS: OnceLock<(LazyHash<FontBook>, Vec<Font>)> = OnceLock::new();
    FONTS.get_or_init(|| {
        let fonts: Vec<Font> = typst_assets::fonts()
            .flat_map(|data| {
                let bytes = Bytes::new(data);
                (0..).map_while(move |i| Font::new(bytes.clone(), i))
            })
            .collect();
        (LazyHash::new(FontBook::from_fonts(&fonts)), fonts)
    })
}

fn library() -> &'static LazyHash<Library> {
    static LIB: OnceLock<LazyHash<Library>> = OnceLock::new();
    LIB.get_or_init(|| LazyHash::new(Library::default()))
}

fn id(path: &str) -> Option<FileId> {
    let vpath = VirtualPath::new(path).ok()?;
    Some(RootedPath::new(VirtualRoot::Project, vpath).intern())
}

struct Mem {
    main: Source,
    files: HashMap<FileId, Bytes>,
}

impl World for Mem {
    fn library(&self) -> &LazyHash<Library> {
        library()
    }
    fn book(&self) -> &LazyHash<FontBook> {
        &fonts().0
    }
    fn main(&self) -> FileId {
        self.main.id()
    }
    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main.id() {
            Ok(self.main.clone())
        } else {
            Err(FileError::AccessDenied)
        }
    }
    fn file(&self, id: FileId) -> FileResult<Bytes> {
        self.files.get(&id).cloned().ok_or(FileError::AccessDenied)
    }
    fn font(&self, index: usize) -> Option<Font> {
        fonts().1.get(index).cloned()
    }
    fn today(&self, _: Option<Duration>) -> Option<Datetime> {
        // no clock: a document's date is the one it is given
        None
    }
}

/// A PDF and what tests and the record need to know of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Pdf {
    pub bytes: Vec<u8>,
    pub pages: usize,
    /// the text of the pages, in reading order of the layout (a line per run of text)
    pub text: String,
}

/// Compile a Typst source to PDF. `files` are the files it may read (`image("logo.png")`),
/// by their path; `ident` identifies the document stably (its kind and arguments), for the
/// PDF's `/ID`.
pub fn pdf(source: &str, files: &[(String, Vec<u8>)], ident: &str) -> Result<Pdf, String> {
    let main_id = id("/main.typ").expect("a valid path");
    let mut map = HashMap::new();
    for (path, bytes) in files {
        let p = if path.starts_with('/') { path.clone() } else { format!("/{path}") };
        let fid = id(&p).ok_or_else(|| format!("`{path}` is not a path a document can read"))?;
        map.insert(fid, Bytes::new(bytes.clone()));
    }
    let world = Mem { main: Source::new(main_id, source.to_string()), files: map };
    let warned = typst::compile::<PagedDocument>(&world);
    let doc = warned.output.map_err(|errs| describe(&errs))?;
    let options = typst_pdf::PdfOptions {
        ident: typst::foundations::Smart::Custom(ident.to_string()),
        creator: typst::foundations::Smart::Custom(Some("Quadra".to_string())),
        timestamp: None,
        ..Default::default()
    };
    let bytes = typst_pdf::pdf(&doc, &options).map_err(|errs| describe(&errs))?;
    let mut text = String::new();
    for page in doc.pages() {
        collect(&page.frame, &mut text);
    }
    Ok(Pdf { bytes, pages: doc.pages().len(), text })
}

fn collect(frame: &Frame, out: &mut String) {
    for (_, item) in frame.items() {
        match item {
            FrameItem::Group(g) => collect(&g.frame, out),
            FrameItem::Text(t) => {
                out.push_str(&t.text);
                out.push('\n');
            }
            _ => {}
        }
    }
}

fn describe(errs: &[SourceDiagnostic]) -> String {
    errs.iter()
        .map(|e| {
            let mut m = e.message.to_string();
            for h in &e.hints {
                m.push_str(&format!(" ({h:?})"));
            }
            m
        })
        .collect::<Vec<_>>()
        .join("; ")
}
