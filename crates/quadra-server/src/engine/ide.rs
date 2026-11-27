//! Editor features for the browser, on the working copy with unsaved buffers on top:
//! go to definition and hover (the same as the language server in VS Code / Cursor), completion
//! that knows the type before the dot, and "inspect": what a field or an expression gives on
//! the real data.

use super::*;
use quadra_sema::{ide, Analysis, Target};
use quadra_syntax::{ast, SyntaxKind::*, SyntaxNode, TextSize};
use quadra_types::T;

pub(super) struct IdeState {
    pub(super) project: Project,
    pub(super) analysis: Analysis,
    pub(super) checked: Checked,
}

pub(super) fn byte_at(text: &str, utf16: usize) -> usize {
    let mut n = 0;
    for (i, c) in text.char_indices() {
        if n >= utf16 {
            return i;
        }
        n += c.len_utf16();
    }
    text.len()
}

pub(super) fn utf16(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].encode_utf16().count()
}

/// What a chain like `customer.address` or `lines.where(…)` is, for completion.
#[derive(Clone, Debug)]
enum Kind {
    Ent(String),
    Coll(String),
    Static(String),
    Value(String),
}

const COLL: &[(&str, &str)] = &[
    ("where", "elements that satisfy a condition"),
    ("sum", "sum of a value"),
    ("count", "how many (optionally: satisfying a condition)"),
    ("any", "true if one satisfies"),
    ("all", "true if all satisfy"),
    ("map", "a value for each"),
    ("first", "the first"),
    ("last", "the last"),
    ("find", "the first that satisfies, or none"),
    ("single", "the only one that satisfies"),
    ("min", "the smallest value"),
    ("max", "the largest value"),
    ("sort_by", "ordered by a value"),
    ("group_by", "groups by one or two keys: (key, key2, items)"),
    ("avg", "the mean of the values"),
    ("distinct", "without repetitions"),
    ("allocate", "split an amount in proportion, to the cent"),
];
const STATIC: &[(&str, &str)] = &[
    ("all", "every record"),
    ("where", "records that satisfy a condition"),
    ("find", "the first that satisfies"),
    ("single", "the only one that satisfies"),
    ("count", "how many"),
];
const TEXT: &[(&str, &str)] = &[
    ("len", "length"),
    ("upper", "UPPER CASE"),
    ("lower", "lower case"),
    ("trim", "without spaces around"),
    ("take", "the first n characters"),
    ("initials", "first letter of each word"),
    ("starts_with", ""),
    ("ends_with", ""),
    ("contains", ""),
];
const NUMERIC: &[(&str, &str)] = &[
    ("round", "rounded (to the currency's decimals)"),
    ("abs", "absolute value"),
];
const DATE: &[(&str, &str)] = &[
    ("month", "year and month"),
    ("year", "the year"),
    ("end_of_month", "last day of the month"),
];
const MEMBER_KEYWORDS: &[&str] = &[
    "check", "warn", "unique", "flow", "action", "frozen", "post", "fn",
];
const EXPR_KEYWORDS: &[&str] = &[
    "if", "else", "and", "or", "not", "is", "in", "none", "true", "false", "self", "match", "when",
    "today", "now", "current",
];
const TOP_KEYWORDS: &[&str] = &[
    "entity",
    "record",
    "trait",
    "enum",
    "extend",
    "ledger",
    "event",
    "fn",
    "const",
    "role",
    "view",
    "report",
    "metric",
    "migration",
    "change",
    "fixture",
    "test",
    "seed",
    "import",
    "from",
];

impl Engine {
    /// The working copy analysed, cached until the buffers or the files change.
    pub(super) fn ide_state(&mut self, buffers: &J) -> Result<Rc<IdeState>, Fail> {
        let key = format!("{}\u{1}{}", self.dev_generation, buffers);
        if let Some((k, s)) = &self.ide_cache {
            if *k == key {
                return Ok(s.clone());
            }
        }
        let project = self.working_project(buffers)?;
        let analysis = project.analyze();
        let checked = quadra_analysis::check(&project);
        let s = Rc::new(IdeState {
            project,
            analysis,
            checked,
        });
        self.ide_cache = Some((key, s.clone()));
        if s.checked.model.is_some() {
            self.ide_types = Some(s.clone());
        }
        Ok(s)
    }

    /// Types come from the last analysis that produced a model: while the user types, the
    /// code is often incomplete.
    pub(super) fn types_for(&self, state: &Rc<IdeState>) -> Rc<IdeState> {
        if state.checked.model.is_some() {
            return state.clone();
        }
        self.ide_types.clone().unwrap_or_else(|| state.clone())
    }

    pub(super) fn dev_ide(&mut self, who: &Principal, body: &J) -> Result<Reply, Fail> {
        self.editor_login(who)?;
        let op = body["op"].as_str().unwrap_or("");
        let path = body["path"].as_str().unwrap_or("");
        let abs = self.root.join(path);
        let state = self.ide_state(&body["buffers"])?;
        let Some(file) = state.project.files().find(|f| f.path == abs) else {
            return Err((404, format!("{path}: not a file of the project")));
        };
        let fid = file.id;
        let text = file.text.clone();
        let byte = byte_at(&text, body["offset"].as_u64().unwrap_or(0) as usize);
        let offset = TextSize::from(byte as u32);
        match op {
            "definition" => {
                let Some(t) = ide::definition(&state.project, &state.analysis, fid, offset) else {
                    return Ok(reply(200, json!({ "found": false })));
                };
                let f = state.project.file(t.file);
                if f.kind == quadra_sema::FileKind::Std {
                    return Ok(reply(200, json!({ "found": false, "std": true })));
                }
                let rel = f
                    .path
                    .strip_prefix(&self.root)
                    .unwrap_or(&f.path)
                    .to_string_lossy()
                    .replace('\\', "/");
                Ok(reply(
                    200,
                    json!({ "found": true, "path": rel, "from": utf16(&f.text, usize::from(t.range.start())), "to": utf16(&f.text, usize::from(t.range.end())) }),
                ))
            }
            "hover" => {
                let Some((range, mut md)) =
                    ide::hover(&state.project, &state.analysis, fid, offset)
                else {
                    return Ok(reply(200, json!({ "found": false })));
                };
                // the type as the checker computed it
                if let Some((_, Target::Member(owner, i))) =
                    ide::target_at(&state.project, &state.analysis, fid, offset)
                {
                    let (o, m) = (state.analysis.decl(owner), state.analysis.member(owner, i));
                    if let Some(t) = state
                        .checked
                        .report
                        .field_types
                        .get(&(o.name.clone(), m.name.clone()))
                    {
                        md.push_str(&format!("\n\nType: `{t}`"));
                    }
                }
                Ok(reply(
                    200,
                    json!({ "found": true, "markdown": md, "from": utf16(&text, usize::from(range.start())), "to": utf16(&text, usize::from(range.end())) }),
                ))
            }
            "complete" => {
                let types = self.types_for(&state);
                Ok(reply(200, self.complete(&state, &types, fid, &text, byte)))
            }
            "inspect" => self.inspect(&state, fid, &text, byte, &body["buffers"]),
            _ => Err((400, "op: definition, hover, complete or inspect".into())),
        }
    }

    /// The entity whose body contains `byte`.
    fn owner_at(root: &SyntaxNode, byte: usize) -> Option<String> {
        let tok = root
            .token_at_offset(TextSize::from(byte as u32))
            .left_biased()?;
        let item = tok
            .parent_ancestors()
            .find(|n| matches!(n.kind(), ENTITY | LEDGER))?;
        if ast::token(&item, EXTEND_KW).is_some()
            || ast::ctx_keyword(&item).as_deref() == Some("patch")
        {
            let p = ast::child(&item, PATH)?;
            return ast::path_segments(&p)
                .into_iter()
                .next()
                .map(|t| t.text().to_string());
        }
        ast::name_text(&item)
    }

    fn field_kind(&self, state: &IdeState, ent: &str, field: &str) -> Option<Kind> {
        let t = state
            .checked
            .report
            .field_types
            .get(&(ent.to_string(), field.to_string()))?;
        fn of(t: &T) -> Kind {
            match t {
                T::Opt(i) => of(i),
                T::Ent(e) => Kind::Ent(e.clone()),
                T::Coll { elem, .. } => match elem.as_ref() {
                    T::Ent(e) => Kind::Coll(e.clone()),
                    other => Kind::Value(format!("[{other}]")),
                },
                other => Kind::Value(other.to_string()),
            }
        }
        Some(of(t))
    }

    /// The kind of a chain of names written before a dot.
    fn chain_kind(
        &self,
        state: &IdeState,
        owner: Option<&str>,
        before: &str,
        chain: &str,
        depth: usize,
    ) -> Option<Kind> {
        if depth > 4 {
            return None;
        }
        // split at dots outside parentheses
        let mut segs: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut level = 0;
        for c in chain.chars() {
            match c {
                '(' => {
                    level += 1;
                    cur.push(c);
                }
                ')' => {
                    level -= 1;
                    cur.push(c);
                }
                '.' if level == 0 => segs.push(std::mem::take(&mut cur)),
                _ => cur.push(c),
            }
        }
        segs.push(cur);
        let model = state.checked.model.as_ref()?;
        let name = |s: &str| {
            s.split('(')
                .next()
                .unwrap_or("")
                .trim_end_matches('?')
                .to_string()
        };
        let first = name(&segs[0]);
        let mut k = if first == "self" {
            Kind::Ent(owner?.to_string())
        } else if let Some(k) = owner.and_then(|o| self.field_kind(state, o, &first)) {
            k
        } else if let Some(k) = self.lambda_param(state, owner, before, &first, depth) {
            k
        } else if model.entity(&first).is_some() {
            Kind::Static(first.clone())
        } else if first == "current" {
            Kind::Static("current".into())
        } else {
            return None;
        };
        for s in &segs[1..] {
            let n = name(s);
            let call = s.contains('(');
            k = match (&k, call) {
                (Kind::Coll(e) | Kind::Static(e), true) => match n.as_str() {
                    "where" | "filter" | "sort_by" | "distinct" | "all" => Kind::Coll(e.clone()),
                    "first" | "last" | "find" | "single" | "min_by" | "max_by" => {
                        Kind::Ent(e.clone())
                    }
                    _ => Kind::Value(n.clone()),
                },
                (Kind::Static(e), false) if e == "current" => match n.as_str() {
                    "user" => Kind::Ent("User".into()),
                    "company" => Kind::Ent("Company".into()),
                    _ => return None,
                },
                (Kind::Ent(e), false) => self.field_kind(state, e, &n)?,
                (Kind::Coll(e), false) => match self.field_kind(state, e, &n)? {
                    Kind::Ent(t) | Kind::Coll(t) => Kind::Coll(t),
                    v => v,
                },
                _ => return None,
            };
        }
        Some(k)
    }

    /// `x` bound by `chain.method(x => …` earlier in the text: an element of `chain`.
    fn lambda_param(
        &self,
        state: &IdeState,
        owner: Option<&str>,
        before: &str,
        name: &str,
        depth: usize,
    ) -> Option<Kind> {
        let pat = format!("({name} =>");
        let pos = before
            .rfind(&pat)
            .or_else(|| before.rfind(&format!("({name}=>")))?;
        let head = &before[..pos];
        // the receiver: back over the method name and the chain before it
        let mut start = head.len();
        let mut level = 0;
        for (i, c) in head.char_indices().rev() {
            match c {
                ')' => level += 1,
                '(' if level > 0 => level -= 1,
                c if level > 0 || c.is_alphanumeric() || c == '_' || c == '.' || c == '?' => {}
                _ => break,
            }
            start = i;
        }
        let full = &head[start..];
        let recv = full.rsplit_once('.').map(|(r, _)| r)?;
        match self.chain_kind(state, owner, &before[..pos], recv, depth + 1)? {
            Kind::Coll(e) | Kind::Static(e) => Some(Kind::Ent(e)),
            k => Some(k),
        }
    }

    fn complete(
        &self,
        state: &IdeState,
        types: &IdeState,
        fid: quadra_sema::FileId,
        text: &str,
        byte: usize,
    ) -> J {
        let before = &text[..byte];
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let line = &before[line_start..];
        let prefix: String = line
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let rest = &line[..line.len() - prefix.len()];
        let root = state.project.file(fid).parse.syntax();
        let owner = Self::owner_at(&root, byte);
        let model = types.checked.model.as_ref();
        let mut items: Vec<(String, &str, String)> = Vec::new(); // label, kind, detail
                                                                 // names from other modules: the import that completing them adds
        let mut edits: HashMap<String, J> = HashMap::new();
        let this = state.project.file(fid);
        let is_module = this.kind == quadra_sema::FileKind::Module;
        let modules: Vec<(quadra_sema::FileId, String)> = state
            .project
            .files()
            .filter(|f| f.kind == quadra_sema::FileKind::Module && f.id != fid)
            .map(|f| (f.id, f.module.clone()))
            .collect();
        let fields_of = |e: &str, items: &mut Vec<(String, &str, String)>| {
            if let Some(ent) = model.and_then(|m| m.entity(e)) {
                for f in model.unwrap().all_fields(ent) {
                    if f.name.starts_with("__") {
                        continue;
                    }
                    let t = types
                        .checked
                        .report
                        .field_types
                        .get(&(ent.name.clone(), f.name.clone()))
                        .map(|t| t.to_string())
                        .unwrap_or_default();
                    items.push((
                        f.name.clone(),
                        if f.formula.is_some() {
                            "formula"
                        } else {
                            "field"
                        },
                        t,
                    ));
                }
                for a in &ent.actions {
                    items.push((a.name.clone(), "action", "action".into()));
                }
                for m in &ent.methods {
                    items.push((m.name.clone(), "method", "fn".into()));
                }
            }
        };
        let trimmed = rest.trim_start();
        if let Some(after) = trimmed.strip_prefix("from ").filter(|_| is_module) {
            match after.split_once(" import ") {
                // `from module import …`: what that module declares
                Some((module, listed)) => {
                    let module = module.trim();
                    let already: Vec<&str> = listed.split(',').map(str::trim).collect();
                    for d in &types.analysis.decls {
                        if types.project.file(d.file).module == module
                            && !already.contains(&d.name.as_str())
                            && types.project.file(d.file).kind == quadra_sema::FileKind::Module
                        {
                            items.push((d.name.clone(), "type", d.kind.describe().to_string()));
                        }
                    }
                }
                // `from …`: the modules
                None => {
                    for (_, m) in &modules {
                        items.push((m.clone(), "module", "module".into()));
                    }
                }
            }
        } else if let Some(recv_text) = rest.strip_suffix('.').or_else(|| rest.strip_suffix("?.")) {
            // after a dot: what the chain before it has
            let mut start = recv_text.len();
            let mut level = 0;
            for (i, c) in recv_text.char_indices().rev() {
                match c {
                    ')' => level += 1,
                    '(' if level > 0 => level -= 1,
                    c if level > 0 || c.is_alphanumeric() || c == '_' || c == '.' || c == '?' => {}
                    _ => break,
                }
                start = i;
            }
            let chain = recv_text[start..].trim_end_matches('?');
            match self.chain_kind(
                types,
                owner.as_deref(),
                &before[..line_start + start],
                chain,
                0,
            ) {
                Some(Kind::Ent(e)) => fields_of(&e, &mut items),
                Some(Kind::Coll(e)) => {
                    items.extend(
                        COLL.iter()
                            .map(|(n, d)| (n.to_string(), "method", d.to_string())),
                    );
                    // a field of the elements: their values together
                    fields_of(&e, &mut items);
                }
                Some(Kind::Static(e)) if e == "current" => {
                    items.push(("user".into(), "field", "User".into()));
                    items.push(("company".into(), "field", "Company".into()));
                }
                Some(Kind::Static(_)) => items.extend(
                    STATIC
                        .iter()
                        .map(|(n, d)| (n.to_string(), "method", d.to_string())),
                ),
                Some(Kind::Value(t)) => {
                    let list: &[(&str, &str)] =
                        if t.starts_with("Text") || t.starts_with("Code") || t.starts_with("Email")
                        {
                            TEXT
                        } else if t.starts_with("Date") {
                            DATE
                        } else if t.starts_with("Money")
                            || t.starts_with("Qty")
                            || t.starts_with("Price")
                            || t.starts_with("Exact")
                            || t == "Decimal"
                            || t == "Int"
                        {
                            NUMERIC
                        } else if t.starts_with('[') {
                            COLL
                        } else {
                            &[]
                        };
                    items.extend(
                        list.iter()
                            .map(|(n, d)| (n.to_string(), "method", d.to_string())),
                    );
                }
                None => {}
            }
        } else if rest.trim().is_empty() && owner.is_none() {
            items.extend(
                TOP_KEYWORDS
                    .iter()
                    .map(|k| (k.to_string(), "keyword", String::new())),
            );
        } else {
            // a name: fields of the record, names bound before, types, functions, keywords
            if let Some(o) = &owner {
                fields_of(o, &mut items);
            }
            let item_start = before
                .rfind("\nentity ")
                .or_else(|| before.rfind("\nextend "))
                .unwrap_or(0);
            let scope = &before[item_start..];
            for w in scope.split(|c: char| {
                !(c.is_alphanumeric() || c == '_' || c == '=' || c == '>' || c == ' ')
            }) {
                if let Some(p) = w.trim().strip_suffix("=>").map(str::trim) {
                    if !p.is_empty() && p.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        items.push((p.to_string(), "variable", "lambda parameter".into()));
                    }
                }
            }
            for l in scope.lines() {
                if let Some(r) = l.trim_start().strip_prefix("let ") {
                    let n: String = r
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if !n.is_empty() {
                        items.push((n, "variable", "let".into()));
                    }
                }
            }
            if let Some(m) = model {
                // in a module only what is in scope; the rest is offered below, with its import
                let visible = |n: &str| !is_module || state.analysis.resolve(fid, n).is_some();
                for e in &m.entities {
                    if !e.name.starts_with("__") && visible(&e.name) {
                        items.push((
                            e.name.clone(),
                            "type",
                            if e.is_ledger {
                                "ledger".into()
                            } else {
                                format!("{:?}", e.kind).to_lowercase()
                            },
                        ));
                    }
                }
                for e in m.enums.iter().filter(|e| visible(&e.name)) {
                    items.push((e.name.clone(), "type", "enum".into()));
                    for v in &e.variants {
                        items.push((v.name.clone(), "variant", e.name.clone()));
                    }
                }
                for f in m.fns.iter().filter(|f| visible(&f.name)) {
                    items.push((f.name.clone(), "function", "fn".into()));
                }
                for c in m.consts.iter().filter(|c| visible(&c.name)) {
                    items.push((c.name.clone(), "constant", "const".into()));
                }
            }
            // declared in another module and not imported here: completing it imports it
            if is_module {
                for d in &types.analysis.decls {
                    let Some((_, module)) = modules.iter().find(|(id, _)| *id == d.file) else {
                        continue;
                    };
                    if state.analysis.resolve(fid, &d.name).is_some()
                        || items.iter().any(|(l, _, _)| *l == d.name)
                    {
                        continue;
                    }
                    if let Some((from, to, insert)) = super::dev::import_edit(text, module, &d.name)
                    {
                        items.push((d.name.clone(), "type", format!("import from {module}")));
                        edits.insert(
                            d.name.clone(),
                            json!({ "from": from, "to": to, "insert": insert }),
                        );
                    }
                }
            }
            if rest.trim().is_empty() && owner.is_some() {
                items.extend(
                    MEMBER_KEYWORDS
                        .iter()
                        .map(|k| (k.to_string(), "keyword", String::new())),
                );
            }
            items.extend(
                EXPR_KEYWORDS
                    .iter()
                    .map(|k| (k.to_string(), "keyword", String::new())),
            );
        }
        // the typed prefix first, then names containing it
        let p = prefix.to_lowercase();
        let mut seen = std::collections::HashSet::new();
        let mut scored: Vec<(u8, usize, String, &str, String)> = items
            .into_iter()
            .filter(|(l, _, _)| seen.insert(l.clone()))
            .filter_map(|(l, k, d)| {
                let ll = l.to_lowercase();
                let score = if p.is_empty() {
                    1
                } else if ll.starts_with(&p) {
                    0
                } else if ll.contains(&p) {
                    2
                } else {
                    return None;
                };
                let rank = match k {
                    "field" | "formula" | "variable" => 0,
                    "method" | "action" => 1,
                    "variant" | "type" => 2,
                    _ => 3,
                };
                Some((score, rank, l, k, d))
            })
            .collect();
        scored.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
        let options: Vec<J> = scored.into_iter().take(120).map(|(_, _, l, k, d)| json!({ "label": l, "kind": k, "detail": d, "edit": edits.get(&l) })).collect();
        json!({ "from": utf16(text, byte - prefix.len()), "options": options })
    }

    /// What a field or an expression gives on the live data.
    fn inspect(
        &mut self,
        state: &IdeState,
        fid: quadra_sema::FileId,
        text: &str,
        byte: usize,
        buffers: &J,
    ) -> Result<Reply, Fail> {
        let root = state.project.file(fid).parse.syntax();
        let offset = TextSize::from(byte as u32);
        let model = state
            .checked
            .model
            .as_ref()
            .ok_or((200, "the project has errors".to_string()))?;
        // a field name: the distribution of its values
        if let Some((_, Target::Member(owner, i))) =
            ide::target_at(&state.project, &state.analysis, fid, offset)
        {
            let (o, m) = (state.analysis.decl(owner), state.analysis.member(owner, i));
            if let Some(e) = model
                .entity(&o.name)
                .filter(|e| e.kind == EntityKind::Entity && !e.is_ledger)
            {
                if model.find_field(e, &m.name).is_some() {
                    let (ent, field) = (e.name.clone(), m.name.clone());
                    return self.distribution(
                        model,
                        &ent,
                        &format!("{ent}.{field}"),
                        &field,
                        "field",
                    );
                }
            }
        }
        // inside a check or a formula: the expression on every record
        let tok = root.token_at_offset(offset).left_biased();
        let node = tok.as_ref().and_then(|t| {
            t.parent_ancestors()
                .find(|n| matches!(n.kind(), CHECK | WARN | FIELD))
        });
        let Some(node) = node else {
            return Ok(reply(
                200,
                json!({ "kind": "none", "message": "Put the cursor on a field, or inside a check or a formula." }),
            ));
        };
        let expr = match node.kind() {
            CHECK | WARN => node
                .children()
                .find(|c| c.kind() != NAME_LIST)
                .map(|c| c.text().to_string()),
            _ => node
                .children()
                .chain(
                    ast::child(&node, MODIFIER_BLOCK)
                        .into_iter()
                        .flat_map(|b| b.children()),
                )
                .find(|c| matches!(c.kind(), VALUE | DEFAULT_VALUE))
                .and_then(|v| v.children().last())
                .map(|c| c.text().to_string()),
        };
        let Some(expr) = expr.map(|e| e.trim().to_string()).filter(|e| !e.is_empty()) else {
            return Ok(reply(
                200,
                json!({ "kind": "none", "message": "Nothing to evaluate here." }),
            ));
        };
        if expr.contains('\n') {
            return Ok(reply(
                200,
                json!({ "kind": "none", "message": "Multi-line expressions and per-group checks are not evaluated yet." }),
            ));
        }
        let Some(owner) = Self::owner_at(&root, byte) else {
            return Ok(reply(
                200,
                json!({ "kind": "none", "message": "Not inside an entity." }),
            ));
        };
        // the expression as a formula of the entity, checked on the working copy
        let mut overlays: HashMap<PathBuf, String> = HashMap::new();
        if let Some(o) = buffers.as_object() {
            for (rel, t) in o {
                if let Ok(p) = super::dev::safe_path_pub(&self.root, rel) {
                    overlays.insert(p, t.as_str().unwrap_or("").to_string());
                }
            }
        }
        overlays.insert(
            self.root.join("tests/__probe__.qd"),
            format!("extend {owner}:\n    __probe = {expr}\n"),
        );
        let project =
            Project::load_with_overlays(&self.root, &overlays).map_err(|e| (500, e.to_string()))?;
        let checked = quadra_analysis::check(&project);
        let probe_errors: Vec<String> = checked
            .diagnostics
            .iter()
            .filter(|d| {
                d.severity == quadra_sema::Severity::Error
                    && project.file(d.file).path.ends_with("__probe__.qd")
            })
            .map(|d| d.message.clone())
            .collect();
        if !probe_errors.is_empty() || checked.model.is_none() {
            return Ok(reply(
                200,
                json!({ "kind": "none", "message": format!("Cannot evaluate: {}", probe_errors.join("; ")) }),
            ));
        }
        let probe_model = checked.model.as_ref().unwrap();
        let label = if expr.len() > 80 {
            format!("{}…", &expr[..80])
        } else {
            expr.clone()
        };
        let _ = text;
        self.distribution(
            probe_model,
            &owner,
            &label,
            "__probe",
            if matches!(node.kind(), CHECK | WARN) {
                "check"
            } else {
                "expression"
            },
        )
    }

    fn distribution(
        &mut self,
        model: &Model,
        ent: &str,
        label: &str,
        field: &str,
        kind: &str,
    ) -> Result<Reply, Fail> {
        let mut i = self.interp(model, None, None)?;
        let mut ids = i.ids(ent, None);
        let total_records = ids.len();
        ids.truncate(3000);
        let mut counts: Vec<(String, usize)> = Vec::new();
        let (mut empty, mut errors) = (0usize, 0usize);
        let mut nums: Vec<rust_decimal::Decimal> = Vec::new();
        let mut unit = String::new();
        let mut yes: Vec<J> = Vec::new();
        let mut no: Vec<J> = Vec::new();
        let (mut n_yes, mut n_no) = (0usize, 0usize);
        let mut first_error = None;
        for id in &ids {
            let rec = V::Ref {
                ent: Rc::from(ent),
                id: *id,
            };
            match i.get(&rec, field, None, None) {
                Err(e) => {
                    errors += 1;
                    first_error.get_or_insert(describe(e));
                }
                Ok(v) if v.is_none() => empty += 1,
                Ok(V::Bool(b)) => {
                    let d = i.display(&rec);
                    if b {
                        n_yes += 1;
                        if yes.len() < 8 {
                            yes.push(json!({ "id": id, "display": d }));
                        }
                    } else {
                        n_no += 1;
                        if no.len() < 8 {
                            no.push(json!({ "id": id, "display": d }));
                        }
                    }
                }
                Ok(v) => {
                    let amount = match &v {
                        V::Money { amt, .. } | V::Price { amt, .. } => Some(*amt),
                        V::Qty { v: q, .. } => Some(*q),
                        other => other.num(),
                    };
                    if let Some(n) = amount {
                        nums.push(n);
                        if unit.is_empty() {
                            unit = match &v {
                                V::Money { cur, .. } => cur.to_string(),
                                V::Qty { unit, .. } => unit.to_string(),
                                _ => String::new(),
                            };
                        }
                    }
                    let d = i.display(&v);
                    match counts.iter_mut().find(|(k, _)| *k == d) {
                        Some(c) => c.1 += 1,
                        None => counts.push((d, 1)),
                    }
                }
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let distinct = counts.len();
        let top: Vec<J> = counts
            .into_iter()
            .take(12)
            .map(|(v, c)| json!({ "value": v, "count": c }))
            .collect();
        let numeric = (!nums.is_empty()).then(|| {
            let sum: rust_decimal::Decimal = nums.iter().copied().sum();
            let min = nums.iter().min().copied().unwrap_or_default();
            let max = nums.iter().max().copied().unwrap_or_default();
            let avg = (sum / rust_decimal::Decimal::from(nums.len())).round_dp(2);
            json!({ "min": min.normalize().to_string(), "max": max.normalize().to_string(), "avg": avg.normalize().to_string(), "sum": sum.normalize().to_string(), "unit": unit })
        });
        let boolean = (n_yes + n_no > 0).then(
            || json!({ "true": n_yes, "false": n_no, "samples_true": yes, "samples_false": no }),
        );
        Ok(reply(
            200,
            json!({
                "kind": kind, "entity": ent, "label": label,
                "records": total_records, "evaluated": ids.len(), "empty": empty, "errors": errors, "error": first_error,
                "distinct": distinct, "top": top, "numeric": numeric, "boolean": boolean,
            }),
        ))
    }
}
