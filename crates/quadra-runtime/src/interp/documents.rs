//! `render(InvoicePdf(inv))`: a document of a `.qdp` file evaluated with its arguments into
//! the blocks of `quadra_print`, every value shown as text, then rendered to PDF. What it
//! gives is a tuple: `name`, `type`, `pages`, `size`, `hash` (SHA-256 of the bytes), `text`
//! (for tests) and, not visible to the code, `content` (the bytes in base64) for what sends
//! it (an email, a file, a printer) or keeps it (`attach`).

use super::{fail, Ctx, Interp, R};
use crate::value::{text, V};
use quadra_ir::*;
use quadra_print::{Align, Block, Column, Doc, Style};
use std::rc::Rc;

impl<'m> Interp<'m> {
    /// Render the document `name` with its arguments: a PDF (`zpl`: a label's ZPL text).
    pub fn render_document(&mut self, name: &str, args: Vec<V>, zpl: bool) -> R<V> {
        let m = self.m;
        let Some(d) = m.documents.iter().find(|d| d.name == name) else {
            return fail(format!("no document `{name}`"));
        };
        let mut ctx = Ctx::default();
        for (i, p) in d.params.iter().enumerate() {
            let v = match args.get(i) {
                Some(v) => v.clone(),
                None => match &p.default {
                    Some(e) => self.eval(e, &mut ctx)?,
                    None => return fail(format!("`{name}` needs its `{}`", p.name)),
                },
            };
            ctx.bind(&p.name, v);
        }
        let shown: Vec<String> = args.iter().map(|a| self.display(a)).collect();
        let ident = format!("{name}({})", shown.join(", "));
        if d.label {
            return self.render_label(d, &mut ctx, &ident, zpl);
        }
        let mut doc = Doc::default();
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        let body = self.blocks(&d.items, &mut ctx, &mut doc, &mut files, true)?;
        doc.body = body;
        let pdf = quadra_print::render(&doc, &files, &ident)
            .or_else(|e| fail(format!("`{name}` could not be laid out: {e}")))?;
        let file = file_name(doc.title.as_deref().unwrap_or(name), "pdf");
        Ok(rendered(
            &file,
            "application/pdf",
            pdf.pages,
            &pdf.bytes,
            &pdf.text,
        ))
    }

    fn render_label(&mut self, d: &Document, _ctx: &mut Ctx, _ident: &str, _zpl: bool) -> R<V> {
        fail(format!(
            "`{}` is a label: labels are rendered by quadra-label (not built in yet)",
            d.name
        ))
    }

    fn opt(&mut self, opts: &[PrintOpt], name: &str, ctx: &mut Ctx) -> R<Option<V>> {
        match opts.iter().find(|o| o.name == name) {
            None => Ok(None),
            Some(o) => match &o.value {
                None => Ok(Some(V::Bool(true))),
                // a word (`align right`, `type qr`): its name, not a variable
                Some(Expr {
                    kind: ExprKind::Name(n),
                    ..
                }) if matches!(name, "align" | "valign" | "type") => Ok(Some(text(n))),
                Some(e) => self.eval(e, ctx).map(Some),
            },
        }
    }

    fn num_opt(&mut self, opts: &[PrintOpt], name: &str, ctx: &mut Ctx) -> R<Option<f64>> {
        Ok(match self.opt(opts, name, ctx)? {
            Some(V::Int(i)) => Some(i as f64),
            Some(V::Dec(d)) => d.to_string().parse().ok(),
            _ => None,
        })
    }

    fn word_opt(&mut self, opts: &[PrintOpt], name: &str, ctx: &mut Ctx) -> R<Option<String>> {
        Ok(match self.opt(opts, name, ctx)? {
            Some(V::Text(t)) => Some(t.to_string()),
            _ => None,
        })
    }

    fn style(&mut self, opts: &[PrintOpt], ctx: &mut Ctx) -> R<Style> {
        Ok(Style {
            size: self.num_opt(opts, "size", ctx)?,
            bold: opts.iter().any(|o| o.name == "bold"),
            italic: opts.iter().any(|o| o.name == "italic"),
            color: self.word_opt(opts, "color", ctx)?,
            font: self.word_opt(opts, "font", ctx)?,
            align: Align::parse(&self.word_opt(opts, "align", ctx)?.unwrap_or_default()),
        })
    }

    fn show(&mut self, v: &V) -> String {
        match v {
            V::None => String::new(),
            V::Text(t) => t.to_string(),
            other => self.display(other),
        }
    }

    fn blocks(
        &mut self,
        items: &[PrintItem],
        ctx: &mut Ctx,
        doc: &mut Doc,
        files: &mut Vec<(String, Vec<u8>)>,
        top: bool,
    ) -> R<Vec<Block>> {
        let mut out = Vec::new();
        for it in items {
            match &it.kind {
                PrintKind::Let { name, value } => {
                    let v = self.eval(value, ctx)?;
                    ctx.bind(name, v);
                }
                PrintKind::If {
                    bind,
                    cond,
                    then,
                    otherwise,
                } => {
                    let c = self.eval(cond, ctx)?;
                    let yes = match bind {
                        Some(_) => !c.is_none(),
                        None => matches!(c, V::Bool(true)),
                    };
                    let branch = if yes { then } else { otherwise };
                    ctx.scopes.push(Default::default());
                    if let (Some(b), true) = (bind, yes) {
                        ctx.bind(b, c.clone());
                    }
                    let r = self.blocks(branch, ctx, doc, files, top);
                    ctx.scopes.pop();
                    out.extend(r?);
                }
                PrintKind::For { name, iter, body } => {
                    let v = self.eval(iter, ctx)?;
                    let items: Vec<V> = v.items().map(|s| s.to_vec()).unwrap_or_default();
                    for x in items {
                        ctx.scopes.push(Default::default());
                        ctx.bind(name, x);
                        let r = self.blocks(body, ctx, doc, files, top);
                        ctx.scopes.pop();
                        out.extend(r?);
                    }
                }
                PrintKind::Element {
                    element,
                    value,
                    opts,
                    binding,
                    body: _,
                    children,
                } => {
                    let value = match value {
                        // `page A4`: the size by its name
                        Some(Expr {
                            kind: ExprKind::Name(n),
                            ..
                        }) if element == "page" => Some(text(n)),
                        Some(e) => Some(self.eval(e, ctx)?),
                        None => None,
                    };
                    match element.as_str() {
                        "title" => doc.title = value.as_ref().map(|v| self.show(v)),
                        "page" => {
                            if let Some(V::Text(p)) = &value {
                                doc.paper = p.to_string();
                            }
                            doc.landscape = opts.iter().any(|o| o.name == "landscape");
                            if let Some(mg) = self.num_opt(opts, "margin", ctx)? {
                                doc.margin = mg;
                            }
                        }
                        "font" => {
                            if let Some(v) = &value {
                                doc.font = self.show(v);
                            }
                            if let Some(s) = self.num_opt(opts, "size", ctx)? {
                                doc.size = s;
                            }
                        }
                        "header" | "footer" => {
                            let bs = self.blocks(children, ctx, doc, files, false)?;
                            if element == "header" {
                                doc.header = bs;
                            } else {
                                doc.footer = bs;
                            }
                        }
                        "heading" => {
                            let t = value.as_ref().map(|v| self.show(v)).unwrap_or_default();
                            let level = self.num_opt(opts, "level", ctx)?.unwrap_or(1.0) as u8;
                            let style = self.style(opts, ctx)?;
                            out.push(Block::Heading {
                                text: t,
                                level,
                                style,
                            });
                        }
                        "text" => {
                            let t = value.as_ref().map(|v| self.show(v)).unwrap_or_default();
                            let style = self.style(opts, ctx)?;
                            out.push(Block::Text { text: t, style });
                        }
                        "pages" => {
                            let t = value.as_ref().map(|v| self.show(v)).unwrap_or_default();
                            let style = self.style(opts, ctx)?;
                            out.push(Block::Pages { format: t, style });
                        }
                        "row" => {
                            let widths = match self.opt(opts, "widths", ctx)? {
                                Some(v) => v
                                    .items()
                                    .map(|xs| {
                                        xs.iter()
                                            .filter_map(|x| match x {
                                                V::Int(i) => Some(*i as f64),
                                                V::Dec(d) => d.to_string().parse().ok(),
                                                _ => None,
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                                None => Vec::new(),
                            };
                            let gap = self.num_opt(opts, "gap", ctx)?.unwrap_or(4.0);
                            let valign = self.word_opt(opts, "valign", ctx)?.unwrap_or_default();
                            let mut cells = Vec::new();
                            for c in children {
                                ctx.scopes.push(Default::default());
                                let r =
                                    self.blocks(std::slice::from_ref(c), ctx, doc, files, false);
                                ctx.scopes.pop();
                                let r = r?;
                                if !r.is_empty() {
                                    cells.push(r);
                                }
                            }
                            out.push(Block::Row {
                                widths,
                                gap,
                                valign,
                                cells,
                            });
                        }
                        "block" => {
                            let width = self.num_opt(opts, "width", ctx)?;
                            let fill = self.word_opt(opts, "fill", ctx)?;
                            let style = self.style(opts, ctx)?;
                            ctx.scopes.push(Default::default());
                            let r = self.blocks(children, ctx, doc, files, false);
                            ctx.scopes.pop();
                            out.push(Block::Group {
                                width,
                                fill,
                                style,
                                children: r?,
                            });
                        }
                        "image" => {
                            let path = value.as_ref().map(|v| self.show(v)).unwrap_or_default();
                            let Some(bytes) = self.m.assets.get(&path) else {
                                return fail(format!("the image `{path}` is not in the project"));
                            };
                            if !files.iter().any(|(p, _)| *p == path) {
                                files.push((path.clone(), bytes.clone()));
                            }
                            let width = self.num_opt(opts, "width", ctx)?;
                            let height = self.num_opt(opts, "height", ctx)?;
                            let align = Align::parse(
                                &self.word_opt(opts, "align", ctx)?.unwrap_or_default(),
                            );
                            out.push(Block::Image {
                                path,
                                width,
                                height,
                                align,
                            });
                        }
                        "space" => {
                            let mm = match &value {
                                Some(V::Int(i)) => *i as f64,
                                Some(V::Dec(d)) => d.to_string().parse().unwrap_or(0.0),
                                _ => 0.0,
                            };
                            out.push(Block::Space(mm));
                        }
                        "line" => {
                            let thickness = self.num_opt(opts, "thickness", ctx)?.unwrap_or(0.5);
                            let color = self.word_opt(opts, "color", ctx)?;
                            out.push(Block::Rule { thickness, color });
                        }
                        "pagebreak" => out.push(Block::PageBreak),
                        "typst" => out.push(Block::Typst(
                            value.as_ref().map(|v| self.show(v)).unwrap_or_default(),
                        )),
                        "table" => {
                            let rows_v: Vec<V> = value
                                .as_ref()
                                .and_then(|v| v.items())
                                .map(|s| s.to_vec())
                                .unwrap_or_default();
                            let header =
                                !matches!(self.opt(opts, "header", ctx)?, Some(V::Bool(false)));
                            let size = self.num_opt(opts, "size", ctx)?;
                            let mut columns = Vec::new();
                            let mut cells_of: Vec<&Expr> = Vec::new();
                            for c in children {
                                if let PrintKind::Element {
                                    element: ce,
                                    value: cv,
                                    opts: co,
                                    body: Some(b),
                                    ..
                                } = &c.kind
                                {
                                    if ce != "column" {
                                        continue;
                                    }
                                    let title = match cv {
                                        Some(e) => {
                                            let v = self.eval(e, ctx)?;
                                            self.show(&v)
                                        }
                                        None => String::new(),
                                    };
                                    let width = self.num_opt(co, "width", ctx)?.unwrap_or(1.0);
                                    let align = Align::parse(
                                        &self.word_opt(co, "align", ctx)?.unwrap_or_default(),
                                    );
                                    let bold = co.iter().any(|o| o.name == "bold");
                                    columns.push(Column {
                                        title,
                                        width,
                                        align,
                                        bold,
                                    });
                                    cells_of.push(b);
                                }
                            }
                            let mut rows = Vec::new();
                            for r in rows_v {
                                ctx.scopes.push(Default::default());
                                if let Some(b) = binding {
                                    ctx.bind(b, r);
                                }
                                let mut row = Vec::new();
                                let mut err = None;
                                for e in &cells_of {
                                    match self.eval(e, ctx) {
                                        Ok(v) => row.push(self.show(&v)),
                                        Err(x) => {
                                            err = Some(x);
                                            break;
                                        }
                                    }
                                }
                                ctx.scopes.pop();
                                if let Some(x) = err {
                                    return Err(x);
                                }
                                rows.push(row);
                            }
                            out.push(Block::Table {
                                columns,
                                rows,
                                header,
                                size,
                            });
                        }
                        "totals" => {
                            let width = self.num_opt(opts, "width", ctx)?;
                            let size = self.num_opt(opts, "size", ctx)?;
                            let mut items = Vec::new();
                            for c in children {
                                if let PrintKind::Element {
                                    element: ce,
                                    value: cv,
                                    opts: co,
                                    body: Some(b),
                                    ..
                                } = &c.kind
                                {
                                    if ce != "item" {
                                        continue;
                                    }
                                    let label = match cv {
                                        Some(e) => {
                                            let v = self.eval(e, ctx)?;
                                            self.show(&v)
                                        }
                                        None => String::new(),
                                    };
                                    let v = self.eval(b, ctx)?;
                                    let shown = self.show(&v);
                                    items.push((label, shown, co.iter().any(|o| o.name == "bold")));
                                }
                            }
                            out.push(Block::Totals { items, width, size });
                        }
                        "barcode" => {
                            return fail(
                                "barcodes in documents come with quadra-label (not built in yet)",
                            );
                        }
                        other => return fail(format!("a document has no element `{other}`")),
                    }
                }
            }
        }
        Ok(out)
    }
}

/// A file name from a title: letters, digits, `-` and `_` (`Invoice INV/2026/00001` →
/// `Invoice-INV-2026-00001.pdf`).
pub(crate) fn file_name(title: &str, ext: &str) -> String {
    let mut s = String::new();
    for c in title.chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
            s.push(c);
        } else if !s.ends_with('-') {
            s.push('-');
        }
    }
    let s = s.trim_matches('-');
    format!("{}.{ext}", if s.is_empty() { "document" } else { s })
}

/// What `render` gives.
pub(crate) fn rendered(name: &str, ctype: &str, pages: usize, bytes: &[u8], text_of: &str) -> V {
    use base64::Engine as _;
    use sha2::Digest as _;
    let hash = hex(&sha2::Sha256::digest(bytes));
    V::Tuple(Rc::new(vec![
        (Some("name".into()), text(name)),
        (Some("type".into()), text(ctype)),
        (Some("pages".into()), V::Int(pages as i64)),
        (Some("size".into()), V::Int(bytes.len() as i64)),
        (Some("hash".into()), text(&hash)),
        (Some("text".into()), text(text_of)),
        (
            Some("content".into()),
            text(&base64::engine::general_purpose::STANDARD.encode(bytes)),
        ),
    ]))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
