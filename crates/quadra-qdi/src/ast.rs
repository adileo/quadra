//! The syntax tree of a `.qdi` file: what the parser reads, the printer writes, and the
//! conversions read from and write to the JSON of apps and saved views.
//!
//! Spans are byte offsets in the file (`start..end`); a tree made from JSON has empty spans.

use serde_json::Value as J;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: u32, end: u32) -> Span {
        Span { start, end }
    }
}

/// A name with where it is written.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Name {
    pub text: String,
    pub span: Span,
}

impl Name {
    pub fn new(text: impl Into<String>) -> Name {
        Name { text: text.into(), span: Span::default() }
    }
}

/// A Quadra expression as the JSON keeps it: one line of text. In the file it may be written
/// over several lines (a method chain, a long list): `map` gives, for each token of the text,
/// its offset in `text` and in the file, so that the checker's messages land on the file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Expr {
    pub text: String,
    /// written `raw "…"`: a text that is not an expression (kept as it is, reported by the
    /// checker)
    pub raw: bool,
    pub span: Span,
    pub map: Vec<(u32, u32)>,
}

impl Expr {
    pub fn new(text: impl Into<String>) -> Expr {
        Expr { text: text.into(), ..Expr::default() }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct File {
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    App(App),
    View(SavedView),
}

/// `role Admin change`, `everyone use`
#[derive(Clone, Debug, PartialEq)]
pub struct Share {
    /// `None`: everyone
    pub role: Option<String>,
    pub change: bool,
}

/// `config a.b: <data>` (a key of the JSON object the block is about: a component's `config`,
/// a saved view's configuration) or `meta a.b: <data>` (a key of the object itself: a
/// component, a page, a sheet, the app's configuration): what has no syntax of its own.
#[derive(Clone, Debug, PartialEq)]
pub struct Raw {
    pub meta: bool,
    pub path: Vec<String>,
    pub value: J,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct App {
    pub comment: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub shares: Vec<Share>,
    pub filters: Vec<FilterDecl>,
    pub lets: Vec<Let>,
    pub pages: Vec<Page>,
    pub raws: Vec<Raw>,
    pub span: Span,
}

/// `view "To ship" of Order:` and its options (`description`, `share`, `type`, and those of an
/// embedded view's rows: `fields`, `where`, `sort`, `group`, `by`, `cards`…)
#[derive(Clone, Debug, PartialEq)]
pub struct SavedView {
    pub comment: Option<String>,
    pub name: String,
    pub entity: Name,
    pub lines: Vec<Line>,
    pub span: Span,
}

/// `filter region "Region": multi Order.customer.region default [3, 5]`
#[derive(Clone, Debug, PartialEq)]
pub struct FilterDecl {
    pub comment: Option<String>,
    pub name: Name,
    pub label: Option<String>,
    pub widget: Name,
    pub entity: Name,
    /// the field path from the entity (`customer.region`)
    pub field: Name,
    pub default: Option<J>,
    /// `meta` lines of the definition (keys without syntax)
    pub raws: Vec<Raw>,
    pub span: Span,
}

/// `let sold = Order.where(…)`
#[derive(Clone, Debug, PartialEq)]
pub struct Let {
    pub comment: Option<String>,
    pub name: Name,
    pub expr: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub comment: Option<String>,
    pub id: Option<Name>,
    pub name: Option<String>,
    pub body: Vec<Block>,
    pub raws: Vec<Raw>,
    pub span: Span,
}

/// What a page, a row or a column holds.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    /// `row [height N]:` children side by side
    Row(Row),
    /// `column [width N]:` children stacked
    Column(Column),
    /// `space N`: N empty columns (in a row) or grid rows (in a page or a column)
    Space(u32, Span),
    Component(Box<Component>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub height: Option<u32>,
    pub children: Vec<Block>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub width: Option<u32>,
    pub children: Vec<Block>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Component {
    pub comment: Option<String>,
    /// the JSON `type` (`filter` is written `filters`; an unknown type `component <type>`)
    pub kind: String,
    pub name: Option<Name>,
    pub title: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// `at x, y size w x h`
    pub at: Option<[u32; 4]>,
    pub lines: Vec<Line>,
    pub span: Span,
}

/// A line of a component's (or a saved view's) block.
#[derive(Clone, Debug, PartialEq)]
pub enum Line {
    /// `key: value` (`names`: the names written in the value, with their places)
    Opt { key: Name, value: Value, span: Span, names: Vec<Name> },
    /// `apply Order: region -> customer.region, state -> none`
    Apply { entity: Name, binds: Vec<(Name, Option<Name>)>, span: Span },
    /// `apply auto: false`
    ApplyAuto(bool, Span),
    /// `compute margin: total - cost` (a computed column of a view)
    Compute { name: Name, expr: Expr, span: Span },
    /// `color "#fee2e2": total > 1000.00 EUR` (rows where it holds get the color)
    Color { color: String, expr: Expr, span: Span },
    /// `default customer: @customer` (a form's starting value)
    Default { field: Name, source: Source, span: Span },
    /// `section "Delivery": address, date` (a form's section)
    Section { title: Option<String>, fields: Vec<String>, span: Span, names: Vec<Name> },
    /// `field qty "Quantity": Qty<pcs> where > 0 default 1 pcs` (a custom form)
    Field(FormField),
    /// `submit "Move":` and the statements it runs (a custom form)
    Submit { label: Option<String>, code: String, span: Span, code_span: Span },
    /// `tab s1 "Pivot" free 200 x 10:` (a spreadsheet's sheet)
    Tab(Tab),
    /// a bare text: a Markdown box's text
    Text(String, Span),
    Raw(Raw),
}

/// The value of `key: value`; which kind a key takes is the option table's (options.rs).
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Expr(Expr),
    Text(String),
    Int(i64),
    Bool(bool),
    /// a word of a fixed set, an entity, a field, a name
    Word(String),
    Names(Vec<String>),
    /// `number 120, customer 200`: keys and data values
    Pairs(Vec<(String, J)>),
    Data(J),
    /// a KPI's or a chart's data: an expression, `cell <component> "<ref>"`, `range …`
    Source(DataSource),
    /// `previous @period`, `target <expr>`
    Compare(Compare),
    /// `@order.ship(carrier=Road)`
    Run(Run),
    /// a button's or form's record, a parameter's value
    From(Source),
    /// `date desc, number`
    Sort(Vec<(String, bool)>),
    /// `date by month, state`
    Group(Vec<(String, Option<String>)>),
    /// `count desc`
    Order(String, bool),
    /// `role Admin change, everyone use`
    Shares(Vec<Share>),
    /// `clear`, `keep`, `open`, `select Transfer as transfer`
    After(After),
    /// `cells, move`: flags that are on
    Flags(Vec<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum DataSource {
    Expr(Expr),
    Cell { component: String, cell: String },
    Range { component: String, range: String },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Compare {
    Previous(String),
    Target(Expr),
}

/// Where a value or a record comes from (the JSON's `{from: …}`).
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// `@name`
    Param(String),
    /// `id 42`
    Id(u64),
    /// `cell pivot "B2"`
    Cell { component: String, cell: String },
    /// a data literal
    Value(J),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// `@order.confirm()`: the entity is the one the app's selection `order` is of
    Param(String),
    /// `Order.confirm()`: no record chosen
    Entity(String),
    /// `Order[@sel]`, `Order[42]`, `Order[cell pivot "B2"]`
    Of(String, Source),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub target: Target,
    pub action: String,
    pub args: Vec<(String, Source)>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum After {
    Clear,
    Keep,
    Open,
    /// publish the record the submit gives as `@name`
    Select { entity: String, name: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct FormField {
    pub comment: Option<String>,
    pub name: Name,
    pub label: Option<String>,
    /// the type as Quadra writes it (`Qty<pcs>`, `Text?`)
    pub ty: Name,
    /// what follows `where` (`> 0`, `0..10`, `v => v.active`)
    pub where_: Option<Expr>,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tab {
    pub comment: Option<String>,
    pub id: Option<Name>,
    pub name: Option<String>,
    pub mode: Option<String>,
    pub rows: Option<u64>,
    pub cols: Option<u64>,
    pub lines: Vec<TabLine>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TabLine {
    /// `A1 bold: "Region"`, `A2: = Order.count()`
    Cell { addr: String, style: Vec<StyleItem>, value: Option<CellValue>, span: Span },
    /// `width A 160, B 90` (px by column)
    Widths(Vec<(String, J)>, Span),
    /// `height 1 30` (px by row)
    Heights(Vec<(String, J)>, Span),
    /// `merge A1:D1, B3:C4`
    Merge(Vec<String>, Span),
    /// `freeze rows 1 cols 2`
    Freeze { rows: Option<i64>, cols: Option<i64>, span: Span },
    Raw(Raw),
}

#[derive(Clone, Debug, PartialEq)]
pub enum CellValue {
    /// a value as typed in the cell (`"42"`, `"Region"`, also `"=…"` written as a string)
    Text(String),
    /// `= <formula>` (the text after `=`, as written)
    Formula(String, Span),
}

/// A part of a cell's style (`s` of the JSON).
#[derive(Clone, Debug, PartialEq)]
pub enum StyleItem {
    /// `bold` (b), `italic` (i), `underline` (u), `strike` (st), `wrap` (wrap)
    Flag(String),
    /// `color "#…"` (fg), `fill "#…"` (bg)
    Color(String, String),
    /// `align center` (ha), `valign top` (va)
    Align(String, String),
    /// `fmt number dec 2 thousands date "d mmm yyyy"`
    Format { kind: String, dec: Option<i64>, thousands: bool, date: Option<String> },
    /// `border {t: thin, c: "#000"}`
    Border(J),
    /// `style {…}`: the keys without syntax
    Style(J),
}
