//! A document as the runtime evaluates it: blocks whose values are already text, written as
//! Typst markup. Every text of the data goes into Typst as a string literal (`#"…"`), never
//! as markup, so what a record holds cannot change the layout.

use std::fmt::Write;

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

impl Align {
    pub fn parse(s: &str) -> Align {
        match s {
            "center" => Align::Center,
            "right" => Align::Right,
            _ => Align::Left,
        }
    }
    fn typst(self) -> &'static str {
        match self {
            Align::Left => "left",
            Align::Center => "center",
            Align::Right => "right",
        }
    }
}

/// How a text looks.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Style {
    pub size: Option<f64>,
    pub bold: bool,
    pub italic: bool,
    pub color: Option<String>,
    pub font: Option<String>,
    pub align: Align,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub title: String,
    pub width: f64,
    pub align: Align,
    pub bold: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Heading {
        text: String,
        level: u8,
        style: Style,
    },
    Text {
        text: String,
        style: Style,
    },
    /// "Page {page} of {pages}", the numbers known only once laid out
    Pages {
        format: String,
        style: Style,
    },
    Row {
        widths: Vec<f64>,
        gap: f64,
        valign: String,
        cells: Vec<Vec<Block>>,
    },
    Group {
        width: Option<f64>,
        fill: Option<String>,
        style: Style,
        children: Vec<Block>,
    },
    /// an image of the project, by the path it is given to Typst with
    Image {
        path: String,
        width: Option<f64>,
        height: Option<f64>,
        align: Align,
    },
    Space(f64),
    Rule {
        thickness: f64,
        color: Option<String>,
    },
    PageBreak,
    Table {
        columns: Vec<Column>,
        rows: Vec<Vec<String>>,
        header: bool,
        size: Option<f64>,
    },
    Totals {
        items: Vec<(String, String, bool)>,
        width: Option<f64>,
        size: Option<f64>,
    },
    /// a barcode as SVG (given to Typst as a file)
    Svg {
        path: String,
        width: Option<f64>,
        height: Option<f64>,
        align: Align,
    },
    Typst(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Doc {
    pub title: Option<String>,
    pub paper: String,
    pub landscape: bool,
    pub margin: f64,
    pub font: String,
    pub size: f64,
    pub header: Vec<Block>,
    pub footer: Vec<Block>,
    pub body: Vec<Block>,
}

impl Default for Doc {
    fn default() -> Doc {
        Doc {
            title: None,
            paper: "a4".into(),
            landscape: false,
            margin: 20.0,
            font: "Libertinus Serif".into(),
            size: 10.0,
            header: Vec::new(),
            footer: Vec::new(),
            body: Vec::new(),
        }
    }
}

/// A Typst string literal of a text.
pub fn lit(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => {}
            '\t' => o.push_str("\\t"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn num(x: f64) -> String {
    // fixed decimals: the same text for the same number on every machine
    let s = format!("{x:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-0" {
        "0".into()
    } else {
        s.to_string()
    }
}

fn paper(p: &str) -> &'static str {
    match p {
        "A3" => "a3",
        "A5" => "a5",
        "A6" => "a6",
        "Letter" => "us-letter",
        "Legal" => "us-legal",
        _ => "a4",
    }
}

fn text_args(s: &Style) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(sz) = s.size {
        args.push(format!("size: {}pt", num(sz)));
    }
    if s.bold {
        args.push("weight: \"bold\"".into());
    }
    if s.italic {
        args.push("style: \"italic\"".into());
    }
    if let Some(c) = &s.color {
        args.push(format!("fill: rgb({})", lit(c)));
    }
    if let Some(f) = &s.font {
        args.push(format!("font: {}", lit(f)));
    }
    args
}

/// `text(…, "the text")`: the body an expression of Typst (a string literal, or a sum of them).
fn text_call(body: &str, s: &Style) -> String {
    let mut args = text_args(s);
    args.push(body.to_string());
    format!("text({})", args.join(", "))
}

fn styled(text: &str, s: &Style) -> String {
    text_call(&lit(text), s)
}

fn block(out: &mut String, b: &Block) {
    match b {
        Block::Heading { text, level, style } => {
            let inner = styled(text, style);
            let _ = writeln!(
                out,
                "#align({}, heading(level: {}, {inner}))",
                style.align.typst(),
                level.clamp(&1, &6)
            );
        }
        Block::Text { text, style } => {
            let _ = writeln!(
                out,
                "#align({}, par({}))",
                style.align.typst(),
                styled(text, style)
            );
        }
        Block::Pages { format, style } => {
            // `{page}` and `{pages}` become the counters, the rest stays text
            let mut parts = Vec::new();
            let mut rest = format.as_str();
            while let Some(i) = rest.find('{') {
                let (before, after) = rest.split_at(i);
                if !before.is_empty() {
                    parts.push(lit(before));
                }
                if let Some(t) = after.strip_prefix("{pages}") {
                    parts.push("str(counter(page).final().first())".into());
                    rest = t;
                } else if let Some(t) = after.strip_prefix("{page}") {
                    parts.push("str(counter(page).get().first())".into());
                    rest = t;
                } else {
                    parts.push(lit("{"));
                    rest = &after[1..];
                }
            }
            if !rest.is_empty() {
                parts.push(lit(rest));
            }
            let joined = if parts.is_empty() {
                "\"\"".to_string()
            } else {
                parts.join(" + ")
            };
            let _ = writeln!(
                out,
                "#align({}, context {})",
                style.align.typst(),
                text_call(&joined, style)
            );
        }
        Block::Row {
            widths,
            gap,
            valign,
            cells,
        } => {
            let cols: Vec<String> = if widths.is_empty() {
                cells.iter().map(|_| "1fr".to_string()).collect()
            } else {
                widths.iter().map(|w| format!("{}fr", num(*w))).collect()
            };
            let _ = write!(
                out,
                "#grid(columns: ({}), column-gutter: {}mm, align: {}, ",
                cols.join(", "),
                num(*gap),
                match valign.as_str() {
                    "middle" => "horizon",
                    "bottom" => "bottom",
                    _ => "top",
                }
            );
            for c in cells {
                out.push('[');
                out.push('\n');
                for b in c {
                    block(out, b);
                }
                out.push_str("], ");
            }
            out.push_str(")\n");
        }
        Block::Group {
            width,
            fill,
            style,
            children,
        } => {
            let mut args = vec![format!("inset: 0mm")];
            if let Some(w) = width {
                args.push(format!("width: {}mm", num(*w)));
            }
            if let Some(f) = fill {
                args.push(format!("fill: rgb({})", lit(f)));
                args[0] = "inset: 3mm".into();
            }
            let _ = write!(
                out,
                "#align({}, block({})[\n",
                style.align.typst(),
                args.join(", ")
            );
            for c in children {
                block(out, c);
            }
            out.push_str("])\n");
        }
        Block::Image {
            path,
            width,
            height,
            align,
        }
        | Block::Svg {
            path,
            width,
            height,
            align,
        } => {
            let mut args = vec![lit(path)];
            if let Some(w) = width {
                args.push(format!("width: {}mm", num(*w)));
            }
            if let Some(h) = height {
                args.push(format!("height: {}mm", num(*h)));
            }
            let _ = writeln!(out, "#align({}, image({}))", align.typst(), args.join(", "));
        }
        Block::Space(mm) => {
            let _ = writeln!(out, "#v({}mm)", num(*mm));
        }
        Block::Rule { thickness, color } => {
            let paint = color
                .as_ref()
                .map(|c| format!("rgb({})", lit(c)))
                .unwrap_or_else(|| "black".into());
            let _ = writeln!(
                out,
                "#line(length: 100%, stroke: {}pt + {paint})",
                num(*thickness)
            );
        }
        Block::PageBreak => out.push_str("#pagebreak()\n"),
        Block::Table {
            columns,
            rows,
            header,
            size,
        } => {
            let widths: Vec<String> = columns
                .iter()
                .map(|c| format!("{}fr", num(c.width)))
                .collect();
            let aligns: Vec<&str> = columns.iter().map(|c| c.align.typst()).collect();
            let size = size
                .map(|s| format!("#set text(size: {}pt)\n", num(s)))
                .unwrap_or_default();
            let _ = write!(out, "#[\n{size}#table(columns: ({}), align: ({}), stroke: (x, y) => if y == 0 {{ (bottom: 0.6pt) }} else {{ (bottom: 0.3pt + luma(200)) }}, inset: (x: 4pt, y: 5pt),\n", widths.join(", "), aligns.join(", "));
            if *header {
                let cells: Vec<String> = columns
                    .iter()
                    .map(|c| {
                        format!(
                            "[#{}]",
                            styled(
                                &c.title,
                                &Style {
                                    bold: true,
                                    ..Style::default()
                                }
                            )
                        )
                    })
                    .collect();
                let _ = writeln!(out, "  table.header({}),", cells.join(", "));
            }
            for r in rows {
                let cells: Vec<String> = r
                    .iter()
                    .zip(columns)
                    .map(|(v, c)| {
                        format!(
                            "[#{}]",
                            styled(
                                v,
                                &Style {
                                    bold: c.bold,
                                    ..Style::default()
                                }
                            )
                        )
                    })
                    .collect();
                let _ = writeln!(out, "  {},", cells.join(", "));
            }
            out.push_str(")\n]\n");
        }
        Block::Totals { items, width, size } => {
            let w = width
                .map(|w| format!("{}mm", num(w)))
                .unwrap_or_else(|| "45%".into());
            let size = size
                .map(|s| format!("#set text(size: {}pt)\n", num(s)))
                .unwrap_or_default();
            let _ = write!(out, "#align(right, block(width: {w})[\n{size}#table(columns: (1fr, auto), align: (left, right), stroke: none, inset: (x: 4pt, y: 3pt),\n");
            for (label, value, bold) in items {
                let st = Style {
                    bold: *bold,
                    ..Style::default()
                };
                let _ = writeln!(
                    out,
                    "  [#{}], [#{}],",
                    styled(label, &st),
                    styled(value, &st)
                );
            }
            out.push_str(")\n])\n");
        }
        Block::Typst(raw) => {
            out.push_str(raw);
            out.push('\n');
        }
    }
}

/// The Typst source of a document.
pub fn to_typst(d: &Doc) -> String {
    let mut out = String::new();
    let title = d.title.as_deref().map(lit).unwrap_or_else(|| "none".into());
    let _ = writeln!(out, "#set document(title: {title}, date: none)");
    let mut page = format!(
        "paper: {}, flipped: {}, margin: {}mm",
        lit(paper(&d.paper)),
        d.landscape,
        num(d.margin)
    );
    for (name, blocks) in [("header", &d.header), ("footer", &d.footer)] {
        if !blocks.is_empty() {
            let mut inner = String::new();
            for b in blocks.iter() {
                block(&mut inner, b);
            }
            let _ = write!(page, ", {name}: [\n{inner}]");
        }
    }
    let _ = writeln!(out, "#set page({page})");
    let _ = writeln!(
        out,
        "#set text(font: {}, size: {}pt, lang: \"en\")",
        lit(&d.font),
        num(d.size)
    );
    out.push_str("#set par(spacing: 0.65em)\n");
    for b in &d.body {
        block(&mut out, b);
    }
    out
}
