//! The options of each kind of component (and of saved views): the word a `.qdi` writes, the
//! kind of value it takes, and the key of the JSON configuration it stands for. The parser,
//! the printer, both conversions and the reference page (the interface files reference (quadra.run/reference/interfaces)) follow these tables, in
//! this order (the canonical order of a block's lines).

/// What an option's value is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vt {
    /// `"text"`
    Text,
    /// a whole number
    Int,
    /// `true`, `false`
    Bool,
    /// one of the words (any name, or a `"string"`, when the list is empty)
    Word(&'static [&'static str]),
    /// `a, b.c` (or a data list `["x y", z]`, `[]`)
    Names,
    /// `key value, key value`
    Pairs,
    /// a Quadra expression (`raw "…"` for a text that is not one)
    Expr,
    /// an expression, `cell <component> "<cell>"` (a KPI) or `range <component> "<range>"` (a chart)
    Source,
    /// `previous @filter`, `target <expression>`
    Compare,
    /// `@order.confirm()`, `Order[@x].ship(carrier=Road)`
    Run,
    /// `@name`, `id 42`, `cell <component> "<cell>"`, a data literal
    From,
    /// `date desc, number`
    Sort,
    /// `date by month, state`
    Group,
    /// `count desc`
    Order,
    /// `role Admin change, everyone use`
    Shares,
    /// `clear`, `keep`, `open`, `select <Entity> as <name>`
    After,
    /// some of the words (`cells, move`): the JSON's `{cells: true, move: true}`
    Flags(&'static [&'static str]),
}

/// An option: its word, its value, the JSON key it sets (a dotted path from the configuration).
#[derive(Clone, Copy, Debug)]
pub struct Opt {
    pub key: &'static str,
    pub vt: Vt,
    pub path: &'static str,
    pub doc: &'static str,
}

const fn o(key: &'static str, vt: Vt, path: &'static str, doc: &'static str) -> Opt {
    Opt { key, vt, path, doc }
}

pub const KPI_STYLES: &[&str] = &["auto", "number", "money", "percent", "compact"];
pub const CHART_KINDS: &[&str] = &["bar", "line", "area", "pie", "scatter"];
pub const BUTTON_STYLES: &[&str] = &["primary", "secondary", "danger"];
pub const FORM_MODES: &[&str] = &["create", "edit", "custom"];
pub const VIEW_TYPES: &[&str] = &["grid", "groups", "kanban"];
pub const FILTER_LAYOUTS: &[&str] = &["row", "column"];
pub const VIEW_EDITS: &[&str] = &["cells", "add", "move"];
pub const WIDGETS: &[&str] = &[
    "select",
    "multi",
    "date_range",
    "number_range",
    "toggle",
    "text",
];

pub const KPI: &[Opt] = &[
    o(
        "source",
        Vt::Source,
        "source",
        "the figure: an expression, or `cell <sheet component> \"<cell>\"`",
    ),
    o("label", Vt::Text, "label", "the text under the figure"),
    o(
        "format",
        Vt::Word(KPI_STYLES),
        "format.style",
        "auto, number, money, percent or compact",
    ),
    o("decimals", Vt::Int, "format.decimals", "the decimals shown"),
    o(
        "compare",
        Vt::Compare,
        "compare",
        "`previous @<date range filter>` or `target <expression>`",
    ),
    o(
        "higher_is_better",
        Vt::Bool,
        "higher_is_better",
        "the colour of a change (true when left out)",
    ),
    o(
        "sparkline",
        Vt::Expr,
        "sparkline",
        "an expression giving numbers or (label, number) pairs",
    ),
];

pub const CHART: &[Opt] = &[
    o(
        "kind",
        Vt::Word(CHART_KINDS),
        "kind",
        "bar, line, area, pie or scatter",
    ),
    o(
        "source",
        Vt::Source,
        "source",
        "the data: an expression giving a list, or `range <sheet component> \"<range>\"`",
    ),
    o("stacked", Vt::Bool, "stacked", "bars or areas stacked"),
    o("horizontal", Vt::Bool, "horizontal", "horizontal bars"),
    o("legend", Vt::Bool, "legend", "the legend shown"),
    o(
        "max_points",
        Vt::Int,
        "max_points",
        "at most this many points (500 when left out)",
    ),
    o("x_title", Vt::Text, "x_title", "the title of the x axis"),
    o("y_title", Vt::Text, "y_title", "the title of the y axis"),
    o(
        "format",
        Vt::Text,
        "format",
        "the format of the values (`currency`, `percent`, …)",
    ),
];

pub const FILTERS: &[Opt] = &[
    o(
        "show",
        Vt::Names,
        "filters",
        "the app's filters this bar shows, in order",
    ),
    o(
        "layout",
        Vt::Word(FILTER_LAYOUTS),
        "layout",
        "row or column",
    ),
];

pub const BUTTON: &[Opt] = &[
    o(
        "label",
        Vt::Text,
        "label",
        "the text on the button (the action's name when left out)",
    ),
    o(
        "run",
        Vt::Run,
        "action",
        "the action or flow transition it runs, on which record, with which parameters",
    ),
    o(
        "confirm",
        Vt::Text,
        "confirm",
        "a question asked before it runs",
    ),
    o(
        "style",
        Vt::Word(BUTTON_STYLES),
        "style",
        "primary (when left out), secondary or danger",
    ),
];

pub const FORM: &[Opt] = &[
    o(
        "entity",
        Vt::Word(&[]),
        "entity",
        "the entity of the record it creates or edits",
    ),
    o(
        "mode",
        Vt::Word(FORM_MODES),
        "mode",
        "create (when left out), edit, or custom (implied by `field` and `submit`)",
    ),
    o(
        "record",
        Vt::From,
        "record",
        "edit: the record (`@order`, `id 42`, `cell …`)",
    ),
    o(
        "fields",
        Vt::Names,
        "sections",
        "the fields, in one section (`section` lines for several)",
    ),
    o("columns", Vt::Int, "columns", "1, 2 (when left out) or 3"),
    o(
        "submit_label",
        Vt::Text,
        "submit_label",
        "the text of the submit button",
    ),
    o(
        "confirm",
        Vt::Text,
        "confirm",
        "custom: a question asked before submitting",
    ),
    o(
        "after",
        Vt::After,
        "after",
        "custom: clear (when left out), keep, open, or `select <Entity> as <name>`",
    ),
    o(
        "message",
        Vt::Expr,
        "message",
        "custom: what is shown after a submit (an f-string)",
    ),
];

/// The options of an embedded view's own configuration (its rows' options follow: `VIEW_BODY`).
pub const VIEW: &[Opt] = &[
    o("of", Vt::Word(&[]), "entity", "the entity of the rows"),
    o("view_id", Vt::Int, "view_id", "a saved view, by id, instead of the options below"),
    o("type", Vt::Word(VIEW_TYPES), "type", "grid, groups or kanban"),
    o("select", Vt::Word(&[]), "select", "the clicked row is `@<name>`"),
    o("page_size", Vt::Int, "page_size", "rows (or cards per column) read at once"),
    o("edit", Vt::Flags(VIEW_EDITS), "edit", "what may be changed through it: `cells`, `add` (a grid), `move` (a board); read-only when left out"),
];

/// The options of a view's rows (an embedded view's `view`, a saved view's configuration).
pub const VIEW_BODY: &[Opt] = &[
    o("fields", Vt::Names, "fields", "the columns, in order"),
    o(
        "widths",
        Vt::Pairs,
        "widths",
        "column widths in px: `number 120, customer 200`",
    ),
    o("where", Vt::Expr, "where", "a condition on the rows"),
    o("sort", Vt::Sort, "order", "`date desc, number`"),
    o("search", Vt::Text, "search", "a full-text search"),
    o(
        "group",
        Vt::Group,
        "group.keys",
        "group by 1 or 2 keys: `customer.region, date by month`",
    ),
    o(
        "aggregates",
        Vt::Pairs,
        "group.aggregates",
        "per group: `total sum, qty avg`",
    ),
    o(
        "pivot",
        Vt::Bool,
        "group.pivot",
        "two keys as a pivot table",
    ),
    o(
        "group_order",
        Vt::Order,
        "group.order",
        "groups sorted by `key`, `count` or an aggregate, `desc`",
    ),
    o(
        "measure",
        Vt::Word(&[]),
        "group.measure",
        "a pivot's cells: `count` (when left out) or an aggregate",
    ),
    o(
        "grouped",
        Vt::Bool,
        "grouped",
        "the groups shown instead of the rows",
    ),
    o("board", Vt::Bool, "board", "shown as a board"),
    o(
        "by",
        Vt::Word(&[]),
        "kanban.field",
        "a board's columns: a state, a choice, a reference",
    ),
    o("cards", Vt::Names, "kanban.cards", "the fields of a card"),
    o(
        "card_title",
        Vt::Word(&[]),
        "kanban.title",
        "a card's title (the record's name when left out)",
    ),
    o("hidden", Vt::Names, "kanban.hidden", "columns not shown"),
    o(
        "collapsed",
        Vt::Names,
        "kanban.collapsed",
        "columns shown narrow",
    ),
    o(
        "wip",
        Vt::Pairs,
        "kanban.wip",
        "at most this many cards in a column (shown, not enforced)",
    ),
    o(
        "sum",
        Vt::Word(&[]),
        "kanban.aggregate",
        "a field summed in each column's header",
    ),
    o(
        "known",
        Vt::Names,
        "known",
        "the model's fields when the view was saved (fields added later are shown)",
    ),
];

/// A saved view's own options (its rows' options follow: `VIEW_BODY`).
pub const SAVED_VIEW: &[Opt] = &[
    o("description", Vt::Text, "", "what it is for"),
    o(
        "share",
        Vt::Shares,
        "",
        "who uses or changes it: roles and everyone",
    ),
    o(
        "type",
        Vt::Word(VIEW_TYPES),
        "",
        "grid (when left out), groups or kanban",
    ),
];

pub const SHEET: &[Opt] = &[o(
    "active",
    Vt::Word(&[]),
    "active",
    "the sheet shown first, by id",
)];

/// The component kinds, as written (`filters` is the JSON's `filter`).
pub const KINDS: &[&str] = &[
    "sheet", "markdown", "chart", "kpi", "filters", "view", "button", "form",
];

/// The JSON `type` of a kind word.
pub fn json_type(kind: &str) -> &str {
    if kind == "filters" {
        "filter"
    } else {
        kind
    }
}

/// The kind word of a JSON `type`; `None`: a type without syntax (written `component <type>`).
pub fn kind_word(ty: &str) -> Option<&'static str> {
    let w = match ty {
        "filter" => "filters",
        // a type of that name has no syntax: `filters` is the JSON's `filter`
        "filters" => return None,
        t => t,
    };
    KINDS.iter().find(|k| **k == w).copied()
}

/// The options of a component's block, by its JSON `type` (an embedded view's are `VIEW` then
/// `VIEW_BODY` under `view.`).
pub fn table(ty: &str) -> &'static [Opt] {
    match ty {
        "kpi" => KPI,
        "chart" => CHART,
        "filter" => FILTERS,
        "button" => BUTTON,
        "form" => FORM,
        "view" => VIEW,
        "sheet" => SHEET,
        _ => &[],
    }
}

/// An option of a component, with the prefix its path has (`view.` for the rows' options of
/// an embedded view).
pub fn find(ty: &str, key: &str) -> Option<(Opt, &'static str)> {
    if let Some(o) = table(ty).iter().find(|o| o.key == key) {
        return Some((*o, ""));
    }
    if ty == "view" {
        return VIEW_BODY
            .iter()
            .find(|o| o.key == key)
            .map(|o| (*o, "view."));
    }
    None
}

/// The options of a saved view.
pub fn find_saved(key: &str) -> Option<Opt> {
    SAVED_VIEW
        .iter()
        .chain(VIEW_BODY.iter())
        .find(|o| o.key == key)
        .copied()
}

/// The values a key takes when left out (dropped from the JSON, as the client fills them in).
pub fn is_default(ty: &str, path: &str, v: &serde_json::Value) -> bool {
    use serde_json::Value as J;
    if v.is_null() {
        return true;
    }
    let empty = |v: &J| match v {
        J::Array(a) => a.is_empty(),
        J::Object(o) => o.is_empty(),
        J::String(s) => s.is_empty(),
        _ => false,
    };
    match (ty, path) {
        ("button", "label") => v == "",
        ("button", "args") => empty(v),
        ("button", "style") => v == "primary",
        ("form", "mode") => v == "create",
        ("form", "sections" | "defaults") => empty(v),
        ("form", "columns") => v == 2,
        ("filter", "show" | "filters") => empty(v),
        ("view", "edit") => empty(v),
        // the rows' options of a view (`viewOf` of the client fills them)
        ("view_body", "widths" | "where" | "order" | "columns" | "colors" | "search" | "drill") => {
            empty(v)
        }
        ("view_body", "grouped" | "board") => v == false,
        _ => false,
    }
}

/// How the reference (the interface files reference (quadra.run/reference/interfaces)) names a kind of value.
fn vt_doc(vt: Vt) -> String {
    match vt {
        Vt::Text => "`\"text\"`".into(),
        Vt::Int => "a whole number".into(),
        Vt::Bool => "`true` · `false`".into(),
        Vt::Word(set) if set.is_empty() => "a name".into(),
        Vt::Word(set) => set
            .iter()
            .map(|w| format!("`{w}`"))
            .collect::<Vec<_>>()
            .join(" · "),
        Vt::Names => "names: `a, b.c`".into(),
        Vt::Pairs => "`key value, …`".into(),
        Vt::Expr => "an expression".into(),
        Vt::Source => "an expression · `cell <c> \"<cell>\"` · `range <c> \"<range>\"`".into(),
        Vt::Compare => "`previous @<filter>` · `target <expr>`".into(),
        Vt::Run => "`<target>.<action>(<param>=<source>, …)`".into(),
        Vt::From => "a source".into(),
        Vt::Sort => "`field [desc], …`".into(),
        Vt::Group => "`field [by <bucket>], …`".into(),
        Vt::Order => "`key` · `count` · an aggregate, `[desc]`".into(),
        Vt::Shares => "`role <R> use|change, everyone use|change`".into(),
        Vt::After => "`clear` · `keep` · `open` · `select <Entity> as <name>`".into(),
        Vt::Flags(set) => format!(
            "some of {}",
            set.iter()
                .map(|w| format!("`{w}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The option tables as the reference shows them (a test keeps the interface files reference (quadra.run/reference/interfaces) the same).
pub fn reference() -> String {
    let mut out = String::new();
    let tables: &[(&str, &[Opt], &str)] = &[
        ("kpi", KPI, ""),
        ("chart", CHART, ""),
        ("filters", FILTERS, ""),
        ("button", BUTTON, ""),
        ("form", FORM, ""),
        ("view", VIEW, ""),
        (
            "view rows (an embedded view's, under `view.`; a saved view's)",
            VIEW_BODY,
            "",
        ),
        ("saved view (`view \"…\" of Entity`)", SAVED_VIEW, ""),
        ("sheet", SHEET, ""),
    ];
    for (name, t, _) in tables {
        out.push_str(&format!(
            "#### {name}\n\n| option | value | JSON | |\n|---|---|---|---|\n"
        ));
        for o in t.iter() {
            let path = if o.path.is_empty() {
                "(the definition)".to_string()
            } else {
                format!("`{}`", o.path)
            };
            out.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                o.key,
                vt_doc(o.vt).replace('|', "\\|"),
                path,
                o.doc.replace('|', "\\|")
            ));
        }
        out.push('\n');
    }
    out
}
