//! Documents and labels of `.qdp` files: their elements and options, the expressions in them
//! typed with the parameters in scope, and `render(…)` / `zpl(…)`.

use super::Checker;
use crate::ty::{Num, T};
use quadra_ir::*;

/// What an element of a document is: where it may be, what value it takes, which options.
struct Spec {
    name: &'static str,
    /// the value after the element's name: `Some(true)` required, `Some(false)` optional
    value: Option<bool>,
    opts: &'static [&'static str],
    /// it has a block of elements (`header:`, `row:`, `table … as l:`)
    children: bool,
    /// it has an expression after `:` (`column "Qty": l.qty`)
    body: bool,
    /// only at the top of a document
    top: bool,
}

const fn spec(
    name: &'static str,
    value: Option<bool>,
    opts: &'static [&'static str],
    children: bool,
    body: bool,
    top: bool,
) -> Spec {
    Spec {
        name,
        value,
        opts,
        children,
        body,
        top,
    }
}

const TEXT_OPTS: &[&str] = &["size", "bold", "italic", "align", "color", "font"];

const DOC: &[Spec] = &[
    spec("title", Some(true), &[], false, false, true),
    spec(
        "page",
        Some(false),
        &["landscape", "margin"],
        false,
        false,
        true,
    ),
    spec("font", Some(true), &["size"], false, false, true),
    spec("header", None, &[], true, false, true),
    spec("footer", None, &[], true, false, true),
    spec(
        "heading",
        Some(true),
        &["level", "align", "color"],
        false,
        false,
        false,
    ),
    spec("text", Some(true), TEXT_OPTS, false, false, false),
    spec(
        "pages",
        Some(true),
        &["size", "align", "color"],
        false,
        false,
        false,
    ),
    spec(
        "row",
        None,
        &["widths", "gap", "valign"],
        true,
        false,
        false,
    ),
    spec(
        "block",
        None,
        &["width", "align", "fill", "color", "size"],
        true,
        false,
        false,
    ),
    spec(
        "image",
        Some(true),
        &["width", "height", "align"],
        false,
        false,
        false,
    ),
    spec("space", Some(true), &[], false, false, false),
    spec("line", None, &["thickness", "color"], false, false, false),
    spec("pagebreak", None, &[], false, false, false),
    spec("table", Some(true), &["size", "header"], true, false, false),
    spec(
        "column",
        Some(true),
        &["width", "align", "bold"],
        false,
        true,
        false,
    ),
    spec("totals", None, &["width", "size"], true, false, false),
    spec("item", Some(true), &["bold", "size"], false, true, false),
    spec(
        "barcode",
        Some(true),
        &["type", "width", "height", "human", "align"],
        false,
        false,
        false,
    ),
    spec("typst", Some(true), &[], false, false, false),
];

const LABEL: &[Spec] = &[
    spec(
        "text",
        Some(true),
        &["x", "y", "size", "bold", "width", "align"],
        false,
        false,
        false,
    ),
    spec(
        "barcode",
        Some(true),
        &["x", "y", "type", "width", "height", "human"],
        false,
        false,
        false,
    ),
    spec(
        "box",
        None,
        &["x", "y", "width", "height", "thickness"],
        false,
        false,
        false,
    ),
    spec(
        "line",
        None,
        &["x", "y", "x2", "y2", "thickness"],
        false,
        false,
        false,
    ),
    spec(
        "image",
        Some(true),
        &["x", "y", "width", "height"],
        false,
        false,
        false,
    ),
];

pub(crate) const SYMBOLOGIES: &[&str] =
    &["code128", "ean13", "gs1_128", "sscc", "qr", "datamatrix"];
pub(crate) const PAGE_SIZES: &[&str] = &["A3", "A4", "A5", "A6", "Letter", "Legal"];
const NUMBER_OPTS: &[&str] = &[
    "size",
    "level",
    "width",
    "height",
    "margin",
    "x",
    "y",
    "x2",
    "y2",
    "thickness",
    "dpi",
    "gap",
];

/// The fields of what `render(…)` gives.
pub(crate) fn rendered() -> T {
    T::Tuple(vec![
        (Some("name".into()), T::Text("Text")),
        (Some("type".into()), T::Text("Text")),
        (Some("pages".into()), T::Num(Num::Int)),
        (Some("size".into()), T::Num(Num::Int)),
        (Some("hash".into()), T::Text("Text")),
        (Some("text".into()), T::Text("Text")),
    ])
}

fn name_of(e: &Expr) -> Option<&str> {
    match &e.kind {
        ExprKind::Name(n) => Some(n.as_str()),
        _ => None,
    }
}

fn literal_text(e: &Expr) -> Option<&str> {
    match &e.kind {
        ExprKind::Lit(Lit::Str(s)) => Some(s.as_str()),
        _ => None,
    }
}

impl<'m> Checker<'m> {
    pub(super) fn check_documents(&mut self) {
        let m = self.m;
        let mut seen = std::collections::HashSet::new();
        for d in &m.documents {
            if !seen.insert(d.name.as_str()) {
                self.error(d.span, format!("`{}` is already declared", d.name));
            }
            let saved = std::mem::replace(&mut self.context, format!("document {}", d.name));
            self.entity = None;
            self.push();
            for p in &d.params {
                let t = match &p.ty {
                    Some(ty) => self.decl_ty(ty, &["self".into()]),
                    None => {
                        self.error(
                            d.span,
                            format!(
                                "the parameter `{}` of `{}` needs a type: `{}: Invoice`",
                                p.name, d.name, p.name
                            ),
                        );
                        T::Unknown
                    }
                };
                self.bind(&p.name, t);
            }
            // a label's size and resolution, on its first line
            let allowed: &[&str] = if d.label {
                &["width", "height", "dpi"]
            } else {
                &[]
            };
            for o in &d.opts {
                if !allowed.contains(&o.name.as_str()) {
                    self.error(o.span, if d.label { format!("a label's first line says its `width`, `height` (millimetres) and `dpi`: not `{}`", o.name) } else { format!("a document's first line has no options: `{}` goes in its body (`page A4 margin 18`)", o.name) });
                    continue;
                }
                self.number_opt(o, true);
            }
            if d.label {
                if !["width", "height"]
                    .iter()
                    .all(|need| d.opts.iter().any(|o| o.name == *need))
                {
                    self.error(d.span, format!("a label says its size: `label {}(…) width 100 height 150 dpi 203:` (millimetres)", d.name));
                }
                if let Some(o) = d.opts.iter().find(|o| o.name == "dpi") {
                    if let Some(v) = o.value.as_ref().and_then(number_of) {
                        if ![152.0, 203.0, 300.0, 600.0].contains(&v) {
                            self.error(
                                o.span,
                                "`dpi` is the printer's resolution: 152, 203, 300 or 600",
                            );
                        }
                    }
                }
            }
            self.print_items(&d.items, d.label, true);
            self.pop();
            self.context = saved;
        }
        for a in &m.attaches {
            self.check_attach(a);
        }
    }

    fn check_attach(&mut self, a: &'m Attach) {
        let m = self.m;
        let Some(d) = m.documents.iter().find(|d| d.name == a.document) else {
            self.error(
                a.span,
                format!(
                    "no document `{}` (declare it in a `.qdp` file: `document {}(x: {}):`)",
                    a.document, a.document, a.entity
                ),
            );
            return;
        };
        let Some(e) = m.entities.iter().find(|e| e.name == a.entity) else {
            self.error(a.span, format!("no entity `{}`", a.entity));
            return;
        };
        let ok_param = d.params.len() == 1
            && matches!(&d.params[0].ty, Some(Ty::Path { segments, .. }) if segments.len() == 1 && segments[0] == e.name);
        if !ok_param {
            self.error(a.span, format!("`attach` renders `{}` from each `{}`: its one parameter is a `{}` (`document {}(x: {}):`)", d.name, e.name, e.name, d.name, e.name));
        }
        match e.fields.iter().find(|f| f.name == a.field) {
            None => self.error(
                a.span,
                format!(
                    "`{}` has no field `{}`: declare `{}: File?` to keep the document",
                    e.name, a.field, a.field
                ),
            ),
            Some(f) => {
                let file = matches!(&f.ty, Some(Ty::Optional(inner)) if matches!(&**inner, Ty::Path { segments, .. } if segments.len() == 1 && segments[0] == "File"));
                if !file {
                    self.error(a.span, format!("`{}.{}` keeps the document: it is a `File?` (empty until the document is rendered)", e.name, a.field));
                } else if let Some(spec) = f.file.as_ref().filter(|s| !s.accepts("application/pdf"))
                {
                    self.error(
                        a.span,
                        format!(
                            "`{}.{}` keeps a PDF document, but it takes {}",
                            e.name,
                            a.field,
                            spec.accepts_text()
                        ),
                    );
                }
            }
        }
        if let Some(by) = &a.signer {
            match m.manifest.signer(by) {
                None => self.error(
                    a.span,
                    format!("no signer `{by}` in quadra.toml ([signers.{by}])"),
                ),
                Some(sd) if sd.p12.is_none() => self.error(
                    a.span,
                    format!("`{by}` has no certificate (`p12`): a PDF is signed with one"),
                ),
                _ => {}
            }
            if d.label {
                self.error(a.span, "a label is not signed: `signed` is for documents");
            }
        }
        if let Some(w) = &a.when {
            let saved = std::mem::replace(&mut self.context, format!("attach {}", a.document));
            self.entity = Some(e);
            self.push();
            self.bind("self", T::Ent(e.name.clone()));
            let t = self.infer(w);
            if !matches!(t.unopt(), T::Bool | T::Unknown) {
                self.error(
                    w.span,
                    "`when` is a condition on the record: `when state is Posted`",
                );
            }
            self.pop();
            self.entity = None;
            self.context = saved;
        }
    }

    fn print_items(&mut self, items: &'m [PrintItem], label: bool, top: bool) {
        for it in items {
            match &it.kind {
                PrintKind::Let { name, value } => {
                    let t = self.infer(value);
                    self.bind(name, t);
                }
                PrintKind::If {
                    bind,
                    cond,
                    then,
                    otherwise,
                } => {
                    let t = self.infer(cond);
                    if bind.is_none() && !matches!(t.unopt(), T::Bool | T::Unknown) {
                        self.error(cond.span, "`if` takes a condition");
                    }
                    self.push();
                    if let Some(b) = bind {
                        self.bind(b, t.unopt().clone());
                    }
                    self.print_items(then, label, top);
                    self.pop();
                    self.push();
                    self.print_items(otherwise, label, top);
                    self.pop();
                }
                PrintKind::For { name, iter, body } => {
                    let t = self.infer(iter);
                    let elem = match t.unopt() {
                        T::Coll { elem, .. } => (**elem).clone(),
                        T::Unknown => T::Unknown,
                        _ => {
                            self.error(
                                iter.span,
                                "`for` goes through a list: `for p in inv.payments:`",
                            );
                            T::Unknown
                        }
                    };
                    self.push();
                    self.bind(name, elem);
                    self.print_items(body, label, top);
                    self.pop();
                }
                PrintKind::Element {
                    element,
                    value,
                    opts,
                    binding,
                    body,
                    children,
                } => {
                    self.print_element(
                        it,
                        element,
                        value.as_ref(),
                        opts,
                        binding.as_deref(),
                        body.as_ref(),
                        children,
                        label,
                        top,
                    );
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn print_element(
        &mut self,
        it: &'m PrintItem,
        element: &str,
        value: Option<&'m Expr>,
        opts: &'m [PrintOpt],
        binding: Option<&str>,
        body: Option<&'m Expr>,
        children: &'m [PrintItem],
        label: bool,
        top: bool,
    ) {
        let specs = if label { LABEL } else { DOC };
        let Some(s) = specs.iter().find(|s| s.name == element) else {
            let names: Vec<&str> = specs.iter().map(|s| s.name).collect();
            self.error(
                it.span,
                format!(
                    "{} has no element `{element}`: {}",
                    if label { "a label" } else { "a document" },
                    names.join(", ")
                ),
            );
            return;
        };
        if s.top && !top {
            self.error(
                it.span,
                format!("`{element}` is said once, at the top of the document"),
            );
        }
        // the elements that hold others: `column` only in a table, `item` only in totals
        let parent_ok = |child: &str| match element {
            "table" => child == "column",
            "totals" => child == "item",
            _ => child != "column" && child != "item",
        };
        match (s.value, value) {
            (Some(true), None) => self.error(
                it.span,
                format!("`{element}` takes a value: {}", example(element)),
            ),
            (None, Some(v)) => self.error(
                v.span,
                format!("`{element}` takes no value: {}", example(element)),
            ),
            _ => {}
        }
        if !s.body && body.is_some() {
            self.error(it.span, format!("`{element}` has nothing after `:`"));
        }
        if s.body && body.is_none() {
            self.error(
                it.span,
                format!(
                    "`{element}` says what it shows after `:`: {}",
                    example(element)
                ),
            );
        }
        if !s.children && !children.is_empty() {
            self.error(it.span, format!("`{element}` holds no other elements"));
        }
        if binding.is_some() && element != "table" {
            self.error(
                it.span,
                "`as` names the rows of a table: `table inv.lines as l:`",
            );
        }
        // the value
        let mut row = T::Unknown;
        if let Some(v) = value {
            match element {
                "page" => {
                    if !name_of(v).is_some_and(|n| PAGE_SIZES.contains(&n)) {
                        self.error(v.span, format!("the size of the page: {}", PAGE_SIZES.join(", ")));
                    }
                }
                "image" | "font" | "typst" | "pages" => {
                    match literal_text(v) {
                        None => self.error(v.span, match element {
                            "image" => "an image is a file of the project, by its path: `image \"assets/logo.png\"`",
                            "font" => "a font by its name: `font \"Libertinus Serif\"` (Libertinus Serif, New Computer Modern, DejaVu Sans Mono)",
                            "typst" => "`typst` takes Typst markup as written, a constant text: `typst \"#lorem(30)\"`",
                            _ => "`pages` takes a text with `{page}` and `{pages}`: `pages \"Page {page} of {pages}\"`",
                        }),
                        Some(t) => {
                            if element == "image" && !(t.ends_with(".png") || t.ends_with(".jpg") || t.ends_with(".jpeg") || t.ends_with(".svg")) {
                                self.error(v.span, "an image is a .png, .jpg or .svg file of the project");
                            }
                            if element == "image" && (t.starts_with('/') || t.contains("..")) {
                                self.error(v.span, "the path of an image is in the project: not from `/`, no `..`");
                            }
                        }
                    }
                }
                "space" => {
                    let t = self.infer(v);
                    if !matches!(t, T::Num(_) | T::Unknown) {
                        self.error(v.span, "`space` takes millimetres: `space 8`");
                    }
                }
                "table" => {
                    let t = self.infer(v);
                    row = match t.unopt() {
                        T::Coll { elem, .. } => (**elem).clone(),
                        T::Unknown => T::Unknown,
                        _ => {
                            self.error(v.span, "a table shows a list, a row for each item: `table inv.lines as l:`");
                            T::Unknown
                        }
                    };
                    if binding.is_none() {
                        self.error(it.span, "a table names its rows: `table inv.lines as l:`");
                    }
                }
                _ => {
                    let t = self.infer(v);
                    if element == "barcode" && !matches!(t.unopt(), T::Text(_) | T::Num(Num::Int) | T::Unknown) {
                        self.error(v.span, "a barcode encodes text (or an integer)");
                    }
                    if matches!(t, T::Coll { .. }) && element != "barcode" {
                        self.error(v.span, format!("`{element}` shows one value: for a list, `for x in …:` or a `table`"));
                    }
                }
            }
        }
        // the options
        for o in opts {
            if !s.opts.contains(&o.name.as_str()) {
                let list = if s.opts.is_empty() {
                    "none".to_string()
                } else {
                    s.opts.join(", ")
                };
                self.error(
                    o.span,
                    format!(
                        "`{element}` has no option `{}` (its options: {list})",
                        o.name
                    ),
                );
                continue;
            }
            match o.name.as_str() {
                n if NUMBER_OPTS.contains(&n) => self.number_opt(o, false),
                "align" | "valign" => {
                    let ok: &[&str] = if o.name == "align" {
                        &["left", "center", "right"]
                    } else {
                        &["top", "middle", "bottom"]
                    };
                    if !o
                        .value
                        .as_ref()
                        .and_then(name_of)
                        .is_some_and(|n| ok.contains(&n))
                    {
                        self.error(o.span, format!("`{}` is {}", o.name, ok.join(", ")));
                    }
                }
                "type" => {
                    if !o
                        .value
                        .as_ref()
                        .and_then(name_of)
                        .is_some_and(|n| SYMBOLOGIES.contains(&n))
                    {
                        self.error(
                            o.span,
                            format!("the kind of barcode: {}", SYMBOLOGIES.join(", ")),
                        );
                    }
                }
                "color" | "fill" => {
                    let ok = o.value.as_ref().and_then(literal_text).is_some_and(|t| {
                        t.len() == 7
                            && t.starts_with('#')
                            && t[1..].chars().all(|c| c.is_ascii_hexdigit())
                    });
                    if !ok {
                        self.error(o.span, format!("`{}` is a colour as `\"#1f4e79\"`", o.name));
                    }
                }
                "font" => {
                    if o.value.as_ref().and_then(literal_text).is_none() {
                        self.error(o.span, "a font by its name: `font \"DejaVu Sans Mono\"`");
                    }
                }
                "widths" => {
                    let ok = match o.value.as_ref().map(|v| &v.kind) {
                        Some(ExprKind::List(items)) => {
                            !items.is_empty()
                                && items
                                    .iter()
                                    .all(|i| number_of(&i.value).is_some_and(|n| n > 0.0))
                        }
                        _ => false,
                    };
                    if !ok {
                        self.error(o.span, "`widths` are the parts of the width each column takes: `widths [2, 1]`");
                    }
                }
                "header" => {
                    if let Some(v) = &o.value {
                        let t = self.infer(v);
                        if !matches!(t, T::Bool | T::Unknown) {
                            self.error(
                                o.span,
                                "`header` is true or false: whether the table has a header row",
                            );
                        }
                    }
                }
                _ => {
                    if let Some(v) = &o.value {
                        self.infer(v);
                    }
                }
            }
        }
        if label {
            for need in ["x", "y"] {
                if !opts.iter().any(|o| o.name == need) {
                    self.error(it.span, format!("an element of a label has its place: `x` and `y`, in millimetres from the top left (`{element} … x 5 y 10`)"));
                    break;
                }
            }
            if element == "box"
                && !(opts.iter().any(|o| o.name == "width")
                    && opts.iter().any(|o| o.name == "height"))
            {
                self.error(it.span, "a box has a `width` and a `height`");
            }
            if element == "line"
                && !(opts.iter().any(|o| o.name == "x2") && opts.iter().any(|o| o.name == "y2"))
            {
                self.error(it.span, "a line goes from `x y` to `x2 y2`");
            }
            if element == "barcode" && !opts.iter().any(|o| o.name == "type") {
                self.error(
                    it.span,
                    format!(
                        "a barcode says its kind: `type sscc` ({})",
                        SYMBOLOGIES.join(", ")
                    ),
                );
            }
        } else if element == "barcode" && !opts.iter().any(|o| o.name == "type") {
            self.error(
                it.span,
                format!(
                    "a barcode says its kind: `type qr` ({})",
                    SYMBOLOGIES.join(", ")
                ),
            );
        }
        // the body after `:` and the children
        self.push();
        if let (Some(b), "table") = (binding, element) {
            self.bind(b, row);
        }
        if let Some(b) = body {
            let t = self.infer(b);
            if matches!(t, T::Coll { .. }) {
                self.error(b.span, "a cell shows one value");
            }
        }
        for c in children {
            if let PrintKind::Element { element: ce, .. } = &c.kind {
                if !parent_ok(ce) {
                    self.error(
                        c.span,
                        match element {
                            "table" => {
                                "a table holds its columns: `column \"Qty\" align right: l.qty`"
                                    .to_string()
                            }
                            "totals" => "totals hold their lines: `item \"Total\" bold: inv.total`"
                                .to_string(),
                            _ if ce == "column" => "`column` is a column of a table".to_string(),
                            _ => "`item` is a line of `totals:`".to_string(),
                        },
                    );
                }
            }
        }
        self.print_items(children, label, false);
        self.pop();
    }

    fn number_opt(&mut self, o: &'m PrintOpt, constant: bool) {
        let Some(v) = &o.value else { return };
        if constant && number_of(v).is_none() {
            self.error(
                o.span,
                format!("`{}` is a number: `{} 100`", o.name, o.name),
            );
            return;
        }
        let t = self.infer(v);
        if !matches!(t, T::Num(Num::Int | Num::Decimal) | T::Unknown) {
            self.error(
                o.span,
                format!(
                    "`{}` is a number ({})",
                    o.name,
                    if o.name == "size" {
                        "points"
                    } else if o.name == "level" {
                        "1 to 3"
                    } else {
                        "millimetres"
                    }
                ),
            );
        }
    }

    /// `render(InvoicePdf(inv))`, `zpl(ParcelLabel(p))`: the document's arguments.
    pub(super) fn check_render(&mut self, n: &str, args: &[Arg], span: Span) -> T {
        let m = self.m;
        let call = match args {
            [a] if a.name.is_none() => match &a.value.kind {
                ExprKind::Call { callee, args } => name_of(callee).map(|d| (d, args)),
                _ => None,
            },
            _ => None,
        };
        let Some((dname, dargs)) = call else {
            self.error(
                span,
                format!(
                    "`{n}` takes a document with its arguments: `{n}({}(inv))`",
                    if n == "zpl" {
                        "ParcelLabel"
                    } else {
                        "InvoicePdf"
                    }
                ),
            );
            for a in args {
                self.infer(&a.value);
            }
            return if n == "zpl" {
                T::Text("Text")
            } else {
                rendered()
            };
        };
        let Some(d) = m.documents.iter().find(|d| d.name == dname) else {
            self.error(
                span,
                format!("no document `{dname}` (declared in a `.qdp` file and imported)"),
            );
            return if n == "zpl" {
                T::Text("Text")
            } else {
                rendered()
            };
        };
        if n == "zpl" && !d.label {
            self.error(span, format!("`{dname}` is a document: `zpl` is for labels (a document is rendered as a PDF with `render`)"));
        }
        // where: after the commit (an outbound, an attach) or in a test, never in a transaction
        let ok = ["outbound ", "test ", "fixture ", "endpoint "]
            .iter()
            .any(|c| self.context.starts_with(c))
            || self.context.is_empty();
        if !ok {
            self.error(span, format!("`{n}` renders a document, which takes time: it runs after the commit, in an `outbound … as e:` block, or with `attach {dname} to …` (not in {})", self.context));
        }
        if dargs.len() != d.params.len() {
            self.error(
                span,
                format!(
                    "`{dname}` takes {} argument(s): {}",
                    d.params.len(),
                    d.params
                        .iter()
                        .map(|p| p.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
        for (i, a) in dargs.iter().enumerate() {
            let t = self.infer(&a.value);
            let p = a
                .name
                .as_deref()
                .and_then(|nm| d.params.iter().find(|p| p.name == nm))
                .or_else(|| d.params.get(i));
            if let Some(Some(ty)) = p.map(|p| &p.ty) {
                let want = self.decl_ty(ty, &["self".into()]);
                self.assign(&want, &t, a.value.span, "argument");
            }
        }
        if n == "zpl" {
            T::Text("Text")
        } else {
            rendered()
        }
    }
}

impl<'m> Checker<'m> {
    /// `sign(doc, "company")`, `p7m`, `xades`, `pgp_sign`, `pgp_encrypt`: what they sign, and a
    /// signer of quadra.toml with the key they need.
    pub(super) fn check_sign(&mut self, n: &str, args: &[Arg], span: Span) -> T {
        let ts: Vec<T> = args.iter().map(|a| self.infer(&a.value)).collect();
        let ret = match n {
            "xades" | "pgp_sign" => T::Text("Text"),
            _ => rendered(),
        };
        if args.len() != 2 || args.iter().any(|a| a.name.is_some()) {
            self.error(span, format!("`{n}` takes what it signs and the signer of quadra.toml: `{n}(doc, \"company\")`"));
            return ret;
        }
        let ok = ["outbound ", "test ", "fixture ", "endpoint "]
            .iter()
            .any(|c| self.context.starts_with(c))
            || self.context.is_empty();
        if !ok {
            self.error(span, format!("`{n}` signs with a key: it runs after the commit, in an `outbound … as e:` block, or in a test (not in {})", self.context));
        }
        let what_ok = match (n, ts[0].unopt()) {
            ("sign", T::Tuple(_)) => true,
            ("xades", T::Ent(r)) => self
                .m
                .entity(r)
                .is_some_and(|e| e.format.as_deref() == Some("xml")),
            ("xades", T::Text(_)) => true,
            (_, T::Tuple(_) | T::Text(_) | T::Unknown) => {
                n != "sign" || matches!(ts[0], T::Unknown)
            }
            (_, T::Ent(r)) => n != "sign" && self.m.entity(r).is_some_and(|e| e.format.is_some()),
            _ => false,
        };
        if !what_ok {
            self.error(args[0].value.span, match n {
                "sign" => "`sign` signs a PDF (PAdES): `sign(render(InvoicePdf(inv)), \"company\")`",
                "xades" => "`xades` signs an XML document: a record `from xml`, or its text",
                _ => "what is signed is a document (`render(…)`), a record with a format, or text",
            });
        }
        let by = match &args[1].value.kind {
            ExprKind::Lit(Lit::Str(s)) => s.clone(),
            _ => {
                self.error(
                    args[1].value.span,
                    format!("the signer is named as written: `{n}(…, \"company\")`"),
                );
                return ret;
            }
        };
        let Some(sd) = self.m.manifest.signer(&by) else {
            self.error(
                args[1].value.span,
                format!("no signer `{by}` in quadra.toml ([signers.{by}])"),
            );
            return ret;
        };
        let need = match n {
            "pgp_sign" => sd.pgp_key.is_some(),
            "pgp_encrypt" => sd.pgp_public.is_some(),
            _ => sd.p12.is_some(),
        };
        if !need {
            self.error(
                args[1].value.span,
                match n {
                    "pgp_sign" => {
                        format!("`{by}` has no `pgp_key`: an OpenPGP signature needs one")
                    }
                    "pgp_encrypt" => {
                        format!("`{by}` has no `pgp_public`: files are encrypted for a public key")
                    }
                    _ => format!("`{by}` has no certificate (`p12`): `{n}` signs with one"),
                },
            );
        }
        ret
    }
}

fn number_of(e: &Expr) -> Option<f64> {
    match &e.kind {
        ExprKind::Lit(Lit::Number(n)) => n.replace('_', "").parse().ok(),
        _ => None,
    }
}

fn example(element: &str) -> &'static str {
    match element {
        "title" => "`title \"Invoice\"`",
        "page" => "`page A4 margin 18`",
        "font" => "`font \"Libertinus Serif\" size 10`",
        "heading" => "`heading \"Invoice\" level 1`",
        "text" => "`text inv.customer.name bold`",
        "pages" => "`pages \"Page {page} of {pages}\"`",
        "image" => "`image \"assets/logo.png\" width 40`",
        "space" => "`space 8`",
        "table" => "`table inv.lines as l:`",
        "column" => "`column \"Qty\" align right: l.qty`",
        "item" => "`item \"Total\" bold: inv.total`",
        "barcode" => "`barcode inv.number type code128 height 12`",
        "typst" => "`typst \"#lorem(30)\"`",
        "box" => "`box x 2 y 2 width 96 height 146`",
        "line" => "`line x 2 y 30 x2 98 y2 30`",
        _ => "",
    }
}
