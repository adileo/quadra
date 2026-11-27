//! Name resolution for a whole project: declarations, imports, scopes, `extend`, members,
//! variants, and checks on every reference that phase 2 can verify without types.

use crate::{Diagnostic, FileId, FileKind, Project, Severity};
use quadra_syntax::{ast, SyntaxKind::*, SyntaxNode, SyntaxToken, TextRange};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeclId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclKind {
    Entity,
    Record,
    Trait,
    Enum,
    Ledger,
    Fn,
    Const,
    Role,
    Fixture,
    View,
    Report,
    Metric,
    Control,
    Migration,
    Change,
    Event,
    /// a `document` or a `label` of a `.qdp` file
    Document,
}

impl DeclKind {
    pub fn is_type(self) -> bool {
        matches!(
            self,
            DeclKind::Entity | DeclKind::Record | DeclKind::Trait | DeclKind::Enum
        )
    }

    pub fn has_members(self) -> bool {
        matches!(
            self,
            DeclKind::Entity
                | DeclKind::Record
                | DeclKind::Trait
                | DeclKind::Ledger
                | DeclKind::Metric
                | DeclKind::Control
        )
    }

    pub fn describe(self) -> &'static str {
        match self {
            DeclKind::Entity => "entity",
            DeclKind::Record => "record",
            DeclKind::Trait => "trait",
            DeclKind::Enum => "enum",
            DeclKind::Ledger => "ledger",
            DeclKind::Fn => "function",
            DeclKind::Const => "constant",
            DeclKind::Role => "role",
            DeclKind::Fixture => "fixture",
            DeclKind::View => "view",
            DeclKind::Report => "report",
            DeclKind::Metric => "metric",
            DeclKind::Control => "control",
            DeclKind::Migration => "migration",
            DeclKind::Change => "change",
            DeclKind::Event => "event",
            DeclKind::Document => "document",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Decl {
    pub name: String,
    pub kind: DeclKind,
    pub file: FileId,
    pub name_range: TextRange,
    pub range: TextRange,
    pub header: String,
    pub doc: Option<String>,
    pub traits: Vec<DeclId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberKind {
    Field,
    Formula,
    Action,
    Method,
    /// action declared only through `flow … by name`
    FlowAction,
    LedgerKey,
    LedgerMeasure,
    LedgerAttr,
}

impl MemberKind {
    pub fn describe(self) -> &'static str {
        match self {
            MemberKind::Field => "field",
            MemberKind::Formula => "formula",
            MemberKind::Action | MemberKind::FlowAction => "action",
            MemberKind::Method => "function",
            MemberKind::LedgerKey => "ledger key",
            MemberKind::LedgerMeasure => "ledger measure",
            MemberKind::LedgerAttr => "ledger attribute",
        }
    }

    pub fn is_action(self) -> bool {
        matches!(self, MemberKind::Action | MemberKind::FlowAction)
    }
}

#[derive(Clone, Debug)]
pub struct Member {
    pub name: String,
    pub kind: MemberKind,
    pub file: FileId,
    pub name_range: TextRange,
    pub header: String,
    pub doc: Option<String>,
    /// Variants this member can hold: inline union types (`Draft | Posted`) or the bare
    /// names returned by `when:` / `match` arms of a formula.
    pub variants: Vec<String>,
    pub via_extend: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Decl(DeclId),
    Member(DeclId, usize),
    Variant(String),
}

#[derive(Clone, Debug)]
pub struct Reference {
    pub file: FileId,
    pub range: TextRange,
    pub target: Target,
}

/// Fields every entity has without declaring them.
pub const IMPLICIT_MEMBERS: &[&str] = &[
    "id",
    "created_at",
    "created_by",
    "updated_at",
    "updated_by",
    "version",
    "history",
];
/// Columns every ledger row has.
pub const IMPLICIT_LEDGER_MEMBERS: &[&str] = &["source", "date"];

pub const BUILTIN_TYPES: &[&str] = &[
    "Text",
    "Code",
    "Int",
    "Decimal",
    "Bool",
    "Date",
    "DateTime",
    "Duration",
    "DateRange",
    "Percent",
    "Email",
    "Phone",
    "VatNumber",
    "Barcode",
    "Role",
    "Money",
    "Price",
    "Qty",
    "Exact",
    "File",
];
/// Generic types whose arguments are units or currencies, checked in phase 3.
const UNIT_GENERICS: &[&str] = &["Money", "Price", "Qty"];

#[derive(Default)]
pub struct Analysis {
    pub diagnostics: Vec<Diagnostic>,
    pub decls: Vec<Decl>,
    /// Own members plus those added by `extend`, in declaration order.
    pub members: HashMap<DeclId, Vec<Member>>,
    /// Names visible in each file: local declarations, imports, and for scripts everything.
    pub scopes: HashMap<FileId, HashMap<String, DeclId>>,
    /// Every variant name, with the places that define it.
    pub variants: HashMap<String, Vec<(FileId, TextRange)>>,
    pub references: Vec<Reference>,
    pub module_files: HashMap<String, FileId>,
    /// Top-level declarations of each module.
    pub module_decls: HashMap<FileId, HashMap<String, DeclId>>,
    /// Currency and unit codes created by `seed:` blocks (`EUR`, `pcs`): usable as values.
    pub unit_codes: HashSet<String>,
}

impl Analysis {
    pub fn decl(&self, id: DeclId) -> &Decl {
        &self.decls[id.0 as usize]
    }

    pub fn member(&self, owner: DeclId, index: usize) -> &Member {
        &self.members[&owner][index]
    }

    pub fn resolve(&self, file: FileId, name: &str) -> Option<DeclId> {
        self.scopes.get(&file)?.get(name).copied()
    }

    /// Find a member on `owner`, its extensions, or its traits.
    pub fn lookup_member(&self, owner: DeclId, name: &str) -> Option<(DeclId, usize)> {
        let mut seen = HashSet::new();
        self.lookup_member_in(owner, name, &mut seen)
    }

    fn lookup_member_in(
        &self,
        owner: DeclId,
        name: &str,
        seen: &mut HashSet<DeclId>,
    ) -> Option<(DeclId, usize)> {
        if !seen.insert(owner) {
            return None;
        }
        if let Some(i) = self
            .members
            .get(&owner)
            .and_then(|ms| ms.iter().position(|m| m.name == name))
        {
            return Some((owner, i));
        }
        for t in self.decl(owner).traits.clone() {
            if let Some(found) = self.lookup_member_in(t, name, seen) {
                return Some(found);
            }
        }
        None
    }

    pub fn has_member(&self, owner: DeclId, name: &str) -> bool {
        if self.lookup_member(owner, name).is_some() {
            return true;
        }
        match self.decl(owner).kind {
            DeclKind::Ledger => IMPLICIT_LEDGER_MEMBERS.contains(&name),
            DeclKind::Entity | DeclKind::Record | DeclKind::Trait => {
                IMPLICIT_MEMBERS.contains(&name)
            }
            _ => false,
        }
    }

    /// All members of `owner` including traits, as (owner, index).
    pub fn all_members(&self, owner: DeclId) -> Vec<(DeclId, usize)> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![owner];
        while let Some(d) = stack.pop() {
            if !seen.insert(d) {
                continue;
            }
            if let Some(ms) = self.members.get(&d) {
                out.extend((0..ms.len()).map(|i| (d, i)));
            }
            stack.extend(self.decl(d).traits.iter().copied());
        }
        out
    }

    /// Variants held by any member of `owner` (used by `on X enters State`).
    pub fn owner_variants(&self, owner: DeclId) -> HashSet<String> {
        self.all_members(owner)
            .into_iter()
            .flat_map(|(d, i)| self.member(d, i).variants.clone())
            .collect()
    }

    /// The declaration that defines `name`, in any module (for "add an import" hints).
    pub fn find_anywhere(&self, name: &str) -> Option<DeclId> {
        let mut found: Vec<DeclId> = self
            .module_decls
            .values()
            .filter_map(|m| m.get(name).copied())
            .collect();
        found.sort();
        found.first().copied()
    }
}

pub(crate) fn analyze(project: &Project) -> Analysis {
    let mut r = Resolver {
        project,
        a: Analysis::default(),
    };
    for f in project.files() {
        for e in &f.parse.errors {
            r.error(f.id, e.range, e.message.clone());
        }
        // `@name` is a parameter of an app: `.qdi` files only (their generated code has none)
        for n in f
            .parse
            .syntax()
            .descendants()
            .filter(|n| n.kind() == PARAM_REF)
        {
            r.error(f.id, n.text_range(), format!("`{}`: `@name` (a filter or a selection of an app) is written only in `.qdi` files", n.text()));
        }
    }
    r.collect_decls();
    r.build_scopes();
    r.resolve_traits();
    r.collect_all_members();
    r.collect_variants();
    r.collect_unit_codes();
    r.check_files();
    r.a.diagnostics.sort_by_key(|d| (d.file, d.range.start()));
    r.a
}

struct Resolver<'p> {
    project: &'p Project,
    a: Analysis,
}

fn is_capitalized(name: &str) -> bool {
    name.chars().next().map_or(false, char::is_uppercase)
}

fn ident(node: &SyntaxNode) -> Option<SyntaxToken> {
    ast::token(node, IDENT)
}

impl<'p> Resolver<'p> {
    fn error(&mut self, file: FileId, range: TextRange, message: String) {
        self.a.diagnostics.push(Diagnostic {
            file,
            range,
            severity: Severity::Error,
            message,
        });
    }

    fn warning(&mut self, file: FileId, range: TextRange, message: String) {
        self.a.diagnostics.push(Diagnostic {
            file,
            range,
            severity: Severity::Warning,
            message,
        });
    }

    fn line(&self, file: FileId, range: TextRange) -> usize {
        let text = &self.project.file(file).text;
        text[..usize::from(range.start()).min(text.len())]
            .matches('\n')
            .count()
            + 1
    }

    fn location(&self, file: FileId, range: TextRange) -> String {
        let f = self.project.file(file);
        let rel = f.path.strip_prefix(&self.project.root).unwrap_or(&f.path);
        format!("{}:{}", rel.display(), self.line(file, range))
    }

    fn reference(&mut self, file: FileId, range: TextRange, target: Target) {
        self.a.references.push(Reference {
            file,
            range,
            target,
        });
    }

    // ---- 1. declarations --------------------------------------------------------------

    fn collect_decls(&mut self) {
        for f in self.project.files() {
            self.a.module_files.insert(f.module.clone(), f.id);
            let mut names: HashMap<String, DeclId> = HashMap::new();
            for item in f.parse.syntax().children() {
                let kind = match item.kind() {
                    ENTITY => {
                        if ast::token(&item, EXTEND_KW).is_some()
                            || ast::ctx_keyword(&item).as_deref() == Some("patch")
                        {
                            continue;
                        }
                        if ast::token(&item, RECORD_KW).is_some() {
                            DeclKind::Record
                        } else if ast::token(&item, TRAIT_KW).is_some() {
                            DeclKind::Trait
                        } else {
                            DeclKind::Entity
                        }
                    }
                    ENUM => DeclKind::Enum,
                    LEDGER => DeclKind::Ledger,
                    FN => DeclKind::Fn,
                    CONST => DeclKind::Const,
                    ROLE => DeclKind::Role,
                    FIXTURE => DeclKind::Fixture,
                    VIEW => DeclKind::View,
                    REPORT => DeclKind::Report,
                    METRIC => DeclKind::Metric,
                    CONTROL => DeclKind::Control,
                    MIGRATION => DeclKind::Migration,
                    CHANGE => DeclKind::Change,
                    EVENT => DeclKind::Event,
                    DOCUMENT => DeclKind::Document,
                    PUBLISH => {
                        // the event it declares, named after the record and the connection
                        let Some((name, range)) = ast::publish_event(&item) else {
                            continue;
                        };
                        let id = DeclId(self.a.decls.len() as u32);
                        self.a.decls.push(Decl {
                            name: name.clone(),
                            kind: DeclKind::Event,
                            file: f.id,
                            name_range: range,
                            range: item.text_range(),
                            header: ast::header(&item),
                            doc: ast::doc_comment(&item),
                            traits: Vec::new(),
                        });
                        if let std::collections::hash_map::Entry::Vacant(v) =
                            names.entry(name.clone())
                        {
                            v.insert(id);
                        } else {
                            self.error(f.id, range, format!("`{name}` is already declared: one `publish` of a record to a connection"));
                        }
                        continue;
                    }
                    _ => continue,
                };
                let Some(tok) = ast::name_token(&item) else {
                    continue;
                };
                let id = DeclId(self.a.decls.len() as u32);
                self.a.decls.push(Decl {
                    name: tok.text().to_string(),
                    kind,
                    file: f.id,
                    name_range: tok.text_range(),
                    range: item.text_range(),
                    header: ast::header(&item),
                    doc: ast::doc_comment(&item),
                    traits: Vec::new(),
                });
                if let Some(prev) = names.get(tok.text()).copied() {
                    let at = self.location(f.id, self.a.decl(prev).name_range);
                    self.error(
                        f.id,
                        tok.text_range(),
                        format!("`{}` is already declared at {at}", tok.text()),
                    );
                } else {
                    names.insert(tok.text().to_string(), id);
                }
            }
            self.a.module_decls.insert(f.id, names);
        }
        // the standard library's declarations are global: a project extends them
        let std_names: HashMap<String, DeclId> = self
            .project
            .files()
            .filter(|f| f.kind == FileKind::Std)
            .flat_map(|f| {
                self.a.module_decls[&f.id]
                    .iter()
                    .map(|(n, id)| (n.clone(), *id))
            })
            .collect();
        for f in self.project.user_files() {
            let mut clashes: Vec<(String, DeclId, DeclId)> = Vec::new();
            for (name, id) in &self.a.module_decls[&f.id] {
                if let Some(std_id) = std_names.get(name) {
                    clashes.push((name.clone(), *id, *std_id));
                }
            }
            clashes.sort_by_key(|c| self.a.decl(c.1).name_range.start());
            for (name, id, std_id) in clashes {
                let module = &self.project.file(self.a.decl(std_id).file).module;
                let hint = match self.a.decl(std_id).kind {
                    DeclKind::Entity | DeclKind::Record | DeclKind::Trait => {
                        format!(": use `extend {name}:` to add fields, rules or actions to it")
                    }
                    _ => ": choose another name".to_string(),
                };
                let range = self.a.decl(id).name_range;
                self.error(
                    f.id,
                    range,
                    format!("`{name}` is declared in the standard library (`{module}`){hint}"),
                );
            }
        }
    }

    // ---- 2. imports and scopes --------------------------------------------------------

    fn build_scopes(&mut self) {
        let mut edges: HashMap<FileId, Vec<(FileId, TextRange)>> = HashMap::new();
        for f in self.project.files() {
            let mut scope = self.a.module_decls[&f.id].clone();
            for imp in f.parse.syntax().children().filter(|n| n.kind() == IMPORT) {
                let Some(path) = ast::child(&imp, PATH) else {
                    continue;
                };
                let module = ast::path_text(&path);
                let Some(&target) = self.a.module_files.get(&module) else {
                    self.error(
                        f.id,
                        path.text_range(),
                        format!("unknown module `{module}`"),
                    );
                    continue;
                };
                if target == f.id {
                    self.error(
                        f.id,
                        path.text_range(),
                        "a module cannot import itself".into(),
                    );
                    continue;
                }
                if f.kind == FileKind::Module && self.project.file(target).kind == FileKind::Script
                {
                    self.error(
                        f.id,
                        path.text_range(),
                        format!("`{module}` is a script (tests, migrations): modules cannot import from it"),
                    );
                    continue;
                }
                edges
                    .entry(f.id)
                    .or_default()
                    .push((target, imp.text_range()));
                for nr in ast::children(&imp, NAME_REF) {
                    let Some(tok) = ident(&nr) else { continue };
                    let name = tok.text();
                    match self.a.module_decls[&target].get(name).copied() {
                        None => {
                            let hint = match self.a.find_anywhere(name) {
                                Some(d) => format!(
                                    " (it is declared in `{}`)",
                                    self.project.file(self.a.decl(d).file).module
                                ),
                                None => String::new(),
                            };
                            self.error(
                                f.id,
                                tok.text_range(),
                                format!("`{name}` is not declared in module `{module}`{hint}"),
                            );
                        }
                        Some(id) => {
                            match scope.get(name).copied() {
                                Some(existing) if existing == id => self.warning(
                                    f.id,
                                    tok.text_range(),
                                    format!("`{name}` is imported more than once"),
                                ),
                                Some(_) => self.error(
                                    f.id,
                                    tok.text_range(),
                                    format!(
                                        "`{name}` is already declared or imported in this module"
                                    ),
                                ),
                                None => {
                                    scope.insert(name.to_string(), id);
                                }
                            }
                            self.reference(f.id, tok.text_range(), Target::Decl(id));
                        }
                    }
                }
            }
            if f.kind == FileKind::Script {
                for other in self.project.files().filter(|o| o.kind == FileKind::Module) {
                    for (n, id) in &self.a.module_decls[&other.id] {
                        scope.entry(n.clone()).or_insert(*id);
                    }
                }
            }
            // the standard library is visible everywhere (local names shadow it)
            for other in self.project.files().filter(|o| o.kind == FileKind::Std) {
                for (n, id) in &self.a.module_decls[&other.id] {
                    scope.entry(n.clone()).or_insert(*id);
                }
            }
            self.a.scopes.insert(f.id, scope);
        }
        self.check_import_cycles(&edges);
    }

    fn check_import_cycles(&mut self, edges: &HashMap<FileId, Vec<(FileId, TextRange)>>) {
        // 0 = unvisited, 1 = in progress, 2 = done
        let mut state: HashMap<FileId, u8> = HashMap::new();
        let mut stack: Vec<FileId> = Vec::new();
        let mut reported: HashSet<Vec<FileId>> = HashSet::new();
        let mut found: Vec<(FileId, TextRange, Vec<FileId>)> = Vec::new();

        fn visit(
            f: FileId,
            edges: &HashMap<FileId, Vec<(FileId, TextRange)>>,
            state: &mut HashMap<FileId, u8>,
            stack: &mut Vec<FileId>,
            found: &mut Vec<(FileId, TextRange, Vec<FileId>)>,
        ) {
            state.insert(f, 1);
            stack.push(f);
            for &(t, range) in edges.get(&f).map(Vec::as_slice).unwrap_or(&[]) {
                match state.get(&t).copied().unwrap_or(0) {
                    0 => visit(t, edges, state, stack, found),
                    1 => {
                        let start = stack.iter().position(|&x| x == t).unwrap();
                        let mut cycle = stack[start..].to_vec();
                        cycle.push(t);
                        found.push((f, range, cycle));
                    }
                    _ => {}
                }
            }
            stack.pop();
            state.insert(f, 2);
        }

        let mut ids: Vec<FileId> = edges.keys().copied().collect();
        ids.sort();
        for f in ids {
            if state.get(&f).copied().unwrap_or(0) == 0 {
                visit(f, edges, &mut state, &mut stack, &mut found);
            }
        }
        for (file, range, cycle) in found {
            let mut key = cycle[..cycle.len() - 1].to_vec();
            key.sort();
            if !reported.insert(key) {
                continue;
            }
            let names: Vec<String> = cycle
                .iter()
                .map(|id| self.project.file(*id).module.clone())
                .collect();
            self.error(
                file,
                range,
                format!(
                    "import cycle: {} (use `extend` in the higher module instead)",
                    names.join(" → ")
                ),
            );
        }
    }

    // ---- 3. traits ----------------------------------------------------------------------

    fn local_decl_of(&self, file: FileId, node: &SyntaxNode) -> Option<DeclId> {
        let name = ast::name_text(node)?;
        let id = *self.a.module_decls.get(&file)?.get(&name)?;
        (self.a.decl(id).range == node.text_range()).then_some(id)
    }

    fn resolve_traits(&mut self) {
        for f in self.project.files() {
            for item in f.parse.syntax().children().filter(|n| n.kind() == ENTITY) {
                let Some(owner) = self.local_decl_of(f.id, &item) else {
                    continue;
                };
                let Some(clause) = ast::child(&item, IS_CLAUSE) else {
                    continue;
                };
                for path in ast::children(&clause, PATH) {
                    if let Some(t) =
                        self.resolve_path_decl(f.id, &path, &[DeclKind::Trait], "a trait")
                    {
                        self.a.decls[owner.0 as usize].traits.push(t);
                    }
                }
            }
        }
    }

    /// Resolve a one-segment PATH to a declaration of one of `kinds`.
    fn resolve_path_decl(
        &mut self,
        file: FileId,
        path: &SyntaxNode,
        kinds: &[DeclKind],
        what: &str,
    ) -> Option<DeclId> {
        let segs = ast::path_segments(path);
        let first = segs.first()?;
        let name = first.text();
        match self.a.resolve(file, name) {
            Some(id) if kinds.contains(&self.a.decl(id).kind) => {
                self.reference(file, first.text_range(), Target::Decl(id));
                Some(id)
            }
            Some(id) => {
                let k = self.a.decl(id).kind.describe();
                self.error(
                    file,
                    first.text_range(),
                    format!("`{name}` is a {k}, expected {what}"),
                );
                None
            }
            None => {
                self.unknown(file, first, what);
                None
            }
        }
    }

    fn unknown(&mut self, file: FileId, tok: &SyntaxToken, what: &str) {
        let what = what.trim_start_matches("a ").trim_start_matches("an ");
        let name = tok.text();
        let hint = match self.a.find_anywhere(name) {
            Some(d) => {
                let module = &self.project.file(self.a.decl(d).file).module;
                format!(": it is declared in `{module}`, add `from {module} import {name}`")
            }
            None => String::new(),
        };
        self.error(
            file,
            tok.text_range(),
            format!("unknown {what} `{name}`{hint}"),
        );
    }

    // ---- 4. members and extend --------------------------------------------------------

    fn collect_all_members(&mut self) {
        // own members first, then extensions (which may target any module)
        for f in self.project.files() {
            for item in f.parse.syntax().children() {
                if !matches!(item.kind(), ENTITY | LEDGER | METRIC | CONTROL) {
                    continue;
                }
                let Some(owner) = self.local_decl_of(f.id, &item) else {
                    continue;
                };
                if let Some(list) = ast::child(&item, MEMBER_LIST) {
                    self.collect_members(owner, f.id, &list, false);
                }
            }
        }
        for f in self.project.files() {
            for item in f.parse.syntax().children().filter(|n| n.kind() == ENTITY) {
                if ast::token(&item, EXTEND_KW).is_none() {
                    continue;
                }
                let Some(path) = ast::child(&item, PATH) else {
                    continue;
                };
                let kinds = [DeclKind::Entity, DeclKind::Record, DeclKind::Trait];
                let Some(owner) =
                    self.resolve_path_decl(f.id, &path, &kinds, "entity, record or trait")
                else {
                    continue;
                };
                if let Some(list) = ast::child(&item, MEMBER_LIST) {
                    self.collect_members(owner, f.id, &list, true);
                }
            }
        }
    }

    fn collect_members(
        &mut self,
        owner: DeclId,
        file: FileId,
        list: &SyntaxNode,
        via_extend: bool,
    ) {
        let mut flow_actions: Vec<(String, TextRange, String)> = Vec::new();
        for m in list.children() {
            let (kind, tok) = match m.kind() {
                FIELD => {
                    let Some(tok) = ast::name_token(&m) else {
                        continue;
                    };
                    let ledger_kw = ast::ctx_keyword(&m);
                    let has_type = type_child(&m).is_some();
                    let kind = match ledger_kw.as_deref() {
                        Some("key") => MemberKind::LedgerKey,
                        Some("measure") => MemberKind::LedgerMeasure,
                        Some("attr") => MemberKind::LedgerAttr,
                        _ if is_formula(&m) => MemberKind::Formula,
                        _ if has_type => MemberKind::Field,
                        _ => {
                            // `number default …`: new default for an inherited field
                            if !self.a.has_member(owner, tok.text()) {
                                let o = self.a.decl(owner).name.clone();
                                self.error(
                                    file,
                                    tok.text_range(),
                                    format!(
                                        "`{}` is not declared on `{o}` or its traits",
                                        tok.text()
                                    ),
                                );
                            } else if let Some((d, i)) = self.a.lookup_member(owner, tok.text()) {
                                self.reference(file, tok.text_range(), Target::Member(d, i));
                            }
                            continue;
                        }
                    };
                    (kind, tok)
                }
                ACTION => match ast::name_token(&m) {
                    Some(t) => (MemberKind::Action, t),
                    None => continue,
                },
                FN => match ast::name_token(&m) {
                    Some(t) => (MemberKind::Method, t),
                    None => continue,
                },
                FLOW => {
                    for t in ast::children(&m, TRANSITION) {
                        if let Some(by) = action_after_by(&t) {
                            flow_actions.push((
                                by.text().to_string(),
                                by.text_range(),
                                ast::header(&t),
                            ));
                        }
                    }
                    continue;
                }
                _ => continue,
            };
            let name = tok.text().to_string();
            // redeclaring a trait member implements it (types are checked by the type checker)
            let in_own = self
                .a
                .members
                .get(&owner)
                .map_or(false, |ms| ms.iter().any(|m| m.name == name));
            if let Some((d, i)) = self.a.lookup_member(owner, &name).filter(|_| in_own) {
                let prev = self.a.member(d, i).clone();
                let at = self.location(prev.file, prev.name_range);
                let o = self.a.decl(owner).name.clone();
                self.error(
                    file,
                    tok.text_range(),
                    format!("`{name}` is already declared on `{o}` at {at}"),
                );
                continue;
            }
            let variants = member_variants(&m);
            self.a.members.entry(owner).or_default().push(Member {
                name,
                kind,
                file,
                name_range: tok.text_range(),
                header: ast::header(&m),
                doc: ast::doc_comment(&m),
                variants,
                via_extend,
            });
        }
        for (name, range, header) in flow_actions {
            if self.a.lookup_member(owner, &name).is_some() {
                continue;
            }
            self.a.members.entry(owner).or_default().push(Member {
                name,
                kind: MemberKind::FlowAction,
                file,
                name_range: range,
                header,
                doc: None,
                variants: Vec::new(),
                via_extend,
            });
        }
    }

    // ---- 5. variants ------------------------------------------------------------------

    fn collect_unit_codes(&mut self) {
        for f in self.project.files() {
            for seed in f.parse.syntax().children().filter(|n| n.kind() == SEED) {
                for call in seed.descendants().filter(|n| n.kind() == CALL_EXPR) {
                    let callee = call
                        .first_child()
                        .filter(|c| c.kind() == NAME_REF)
                        .and_then(|c| ident(&c));
                    if !matches!(
                        callee.as_ref().map(|t| t.text()),
                        Some("Currency") | Some("Uom")
                    ) {
                        continue;
                    }
                    for arg in call.descendants().filter(|n| n.kind() == ARG) {
                        if ast::name_text(&arg).as_deref() != Some("code") {
                            continue;
                        }
                        if let Some(lit) = arg
                            .descendants_with_tokens()
                            .filter_map(|e| e.into_token())
                            .find(|t| t.kind() == STRING)
                        {
                            self.a
                                .unit_codes
                                .insert(lit.text().trim_matches('"').to_string());
                        }
                    }
                }
            }
        }
    }

    fn collect_variants(&mut self) {
        for f in self.project.files() {
            for node in f
                .parse
                .syntax()
                .descendants()
                .filter(|n| n.kind() == VARIANT)
            {
                if let Some(tok) = ast::name_token(&node) {
                    let entry = self.a.variants.entry(tok.text().to_string()).or_default();
                    if !entry.contains(&(f.id, tok.text_range())) {
                        entry.push((f.id, tok.text_range()));
                    }
                }
            }
        }
    }

    // ---- 6. checks ----------------------------------------------------------------------

    fn check_files(&mut self) {
        for f in self.project.files() {
            let root = f.parse.syntax();
            let mut preorder = root.preorder();
            while let Some(event) = preorder.next() {
                let rowan::WalkEvent::Enter(node) = event else {
                    continue;
                };
                if is_excluded(&node) {
                    preorder.skip_subtree();
                    continue;
                }
                self.check_node(f.id, &node);
            }
        }
    }

    fn enclosing_owner(&mut self, file: FileId, node: &SyntaxNode) -> Option<DeclId> {
        let item = node
            .ancestors()
            .find(|n| matches!(n.kind(), ENTITY | LEDGER))?;
        if ast::token(&item, EXTEND_KW).is_some() {
            let path = ast::child(&item, PATH)?;
            let seg = ast::path_segments(&path).into_iter().next()?;
            return self.a.resolve(file, seg.text());
        }
        self.local_decl_of(file, &item)
    }

    fn check_node(&mut self, file: FileId, node: &SyntaxNode) {
        match node.kind() {
            TYPE_PATH => self.check_type_path(file, node),
            TYPE_UNION => self.error(
                file,
                node.text_range(),
                "anonymous unions are not allowed: use `enum(A, B)` for a set of values, \
                 or a trait for references to different entities"
                    .into(),
            ),
            IS_CLAUSE => {
                let parent = node.parent().map(|p| p.kind());
                for path in ast::children(node, PATH) {
                    if parent == Some(ROLE) {
                        self.resolve_path_decl(file, &path, &[DeclKind::Role], "a role");
                    }
                    // entity traits were resolved (and reported) in resolve_traits
                }
            }
            REACTION => self.check_reaction(file, node),
            POST => {
                if let Some(path) = ast::child(node, PATH) {
                    self.resolve_path_decl(file, &path, &[DeclKind::Ledger], "a ledger");
                }
            }
            FIXTURE => {
                if let Some(path) = ast::child(node, PATH) {
                    self.resolve_path_decl(file, &path, &[DeclKind::Fixture], "a fixture");
                }
            }
            KEYWORD_STMT if ast::ctx_keyword(node).as_deref() == Some("use") => {
                if let Some(tok) = node
                    .descendants()
                    .find(|n| n.kind() == NAME_REF)
                    .and_then(|n| ident(&n))
                {
                    self.check_decl_name(file, &tok, &[DeclKind::Fixture], "fixture");
                }
            }
            RULE => {
                for path in ast::children(node, PATH) {
                    self.check_member_path(file, &path, true);
                }
            }
            REPORT_ITEM => {
                if ast::token(node, FROM_KW).is_some() {
                    if let Some(path) = ast::child(node, PATH) {
                        let kinds = [DeclKind::Entity, DeclKind::Ledger];
                        self.resolve_path_decl(file, &path, &kinds, "an entity");
                    }
                }
            }
            META => {
                if let Some(path) = ast::child(node, PATH) {
                    self.resolve_path_decl(file, &path, &[DeclKind::Role], "a role");
                }
            }
            MIGRATION_STEP => {
                let paths: Vec<SyntaxNode> = ast::children(node, PATH).collect();
                let rename = ast::ctx_keyword(node).as_deref() == Some("rename");
                for (i, path) in paths.iter().enumerate() {
                    // the old name of a rename no longer exists
                    let check_member = !(rename && i == 0);
                    self.check_member_path(file, path, check_member);
                }
            }
            FLOW => self.check_flow(file, node),
            UNIQUE | FROZEN | CHECK => {
                let Some(owner) = self.enclosing_owner(file, node) else {
                    return;
                };
                let mut keyword = String::new();
                for el in node.children_with_tokens() {
                    match el {
                        rowan::NodeOrToken::Token(t) if t.kind() == CONTEXT_KW => {
                            keyword = t.text().to_string()
                        }
                        rowan::NodeOrToken::Node(list) if list.kind() == NAME_LIST => {
                            for nr in ast::children(&list, NAME_REF) {
                                let Some(tok) = ident(&nr) else { continue };
                                if !self.check_member_name(file, owner, &tok) || keyword != "live" {
                                    continue;
                                }
                                if let Some((d, i)) = self.a.lookup_member(owner, tok.text()) {
                                    if self.a.member(d, i).kind != MemberKind::Formula {
                                        self.error(
                                            file,
                                            tok.text_range(),
                                            format!(
                                                "`live` lists formulas; `{}` is a stored field",
                                                tok.text()
                                            ),
                                        );
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            NAME_REF => self.check_name_ref(file, node),
            _ => {}
        }
    }

    fn check_decl_name(&mut self, file: FileId, tok: &SyntaxToken, kinds: &[DeclKind], what: &str) {
        match self.a.resolve(file, tok.text()) {
            Some(id) if kinds.contains(&self.a.decl(id).kind) => {
                self.reference(file, tok.text_range(), Target::Decl(id));
            }
            Some(id) => {
                let k = self.a.decl(id).kind.describe();
                self.error(
                    file,
                    tok.text_range(),
                    format!("`{}` is a {k}, expected a {what}", tok.text()),
                );
            }
            None => self.unknown(file, tok, what),
        }
    }

    fn check_member_name(&mut self, file: FileId, owner: DeclId, tok: &SyntaxToken) -> bool {
        if let Some((d, i)) = self.a.lookup_member(owner, tok.text()) {
            self.reference(file, tok.text_range(), Target::Member(d, i));
            return true;
        }
        if self.a.has_member(owner, tok.text()) {
            return true; // implicit
        }
        let o = self.a.decl(owner).name.clone();
        self.error(
            file,
            tok.text_range(),
            format!("`{o}` has no member `{}`", tok.text()),
        );
        false
    }

    /// `Entity.member…` paths (roles, migrations). Lower-case single names are built-ins.
    fn check_member_path(&mut self, file: FileId, path: &SyntaxNode, check_member: bool) {
        let segs = ast::path_segments(path);
        let Some(first) = segs.first() else { return };
        if !is_capitalized(first.text()) {
            return;
        }
        let Some(owner) = self.a.resolve(file, first.text()) else {
            self.unknown(file, first, "name");
            return;
        };
        self.reference(file, first.text_range(), Target::Decl(owner));
        if !check_member || !self.a.decl(owner).kind.has_members() {
            return;
        }
        if let Some(second) = segs.get(1) {
            self.check_member_name(file, owner, second);
        }
    }

    fn check_reaction(&mut self, file: FileId, node: &SyntaxNode) {
        let Some(path) = ast::child(node, PATH) else {
            return;
        };
        let segs = ast::path_segments(&path);
        let Some(first) = segs.first() else { return };
        // `on OrderShipped delivered as d:`: the outcome of an event's delivery
        let words: Vec<String> = node
            .children_with_tokens()
            .filter_map(|t| t.into_token())
            .map(|t| t.text().to_string())
            .collect();
        // `on OrderIn rejected as r:`: an inbound, checked by the type checker
        if words.iter().any(|w| w == "rejected") {
            return;
        }
        let outcome = words
            .iter()
            .any(|w| matches!(w.as_str(), "delivered" | "failed" | "expired" | "skipped"));
        if outcome {
            match self
                .a
                .resolve(file, first.text())
                .filter(|d| self.a.decl(*d).kind == DeclKind::Event)
            {
                Some(ev) => self.reference(file, first.text_range(), Target::Decl(ev)),
                None => self.unknown(file, first, "event"),
            }
            return;
        }
        let kinds = [DeclKind::Entity, DeclKind::Record];
        let Some(owner) = self
            .a
            .resolve(file, first.text())
            .filter(|d| kinds.contains(&self.a.decl(*d).kind))
        else {
            self.unknown(file, first, "entity");
            return;
        };
        self.reference(file, first.text_range(), Target::Decl(owner));
        match ast::ctx_keyword(node).as_deref() {
            Some("before") => {
                let Some(action) = segs.get(1) else {
                    self.error(file, path.text_range(), "expected `Entity.action`".into());
                    return;
                };
                if self.check_member_name(file, owner, action) {
                    if let Some((d, i)) = self.a.lookup_member(owner, action.text()) {
                        if !self.a.member(d, i).kind.is_action() {
                            self.error(
                                file,
                                action.text_range(),
                                format!("`{}` is not an action", action.text()),
                            );
                        }
                    }
                }
            }
            _ => {
                let enters = node
                    .children_with_tokens()
                    .filter_map(|e| e.into_token())
                    .any(|t| t.kind() == CONTEXT_KW && t.text() == "enters");
                if enters {
                    if let Some(state) = ast::child(node, NAME_REF).and_then(|n| ident(&n)) {
                        let variants = self.a.owner_variants(owner);
                        if variants.contains(state.text()) {
                            self.reference(
                                file,
                                state.text_range(),
                                Target::Variant(state.text().into()),
                            );
                        } else {
                            let o = self.a.decl(owner).name.clone();
                            self.error(
                                file,
                                state.text_range(),
                                format!("`{o}` has no state `{}`", state.text()),
                            );
                        }
                    }
                }
            }
        }
    }

    fn check_flow(&mut self, file: FileId, node: &SyntaxNode) {
        let Some(owner) = self.enclosing_owner(file, node) else {
            return;
        };
        let Some(field) = ast::child(node, NAME_REF).and_then(|n| ident(&n)) else {
            return;
        };
        if !self.check_member_name(file, owner, &field) {
            return;
        }
        let Some((d, i)) = self.a.lookup_member(owner, field.text()) else {
            return;
        };
        let states: HashSet<String> = self.a.member(d, i).variants.iter().cloned().collect();
        if states.is_empty() {
            self.error(
                file,
                field.text_range(),
                format!(
                    "`{f}` must have an inline enum type to be used in a flow: declare it as `{f}: enum(Draft, Posted) default Draft`",
                    f = field.text()
                ),
            );
            return;
        }
        for t in ast::children(node, TRANSITION) {
            // states are the names before `by` / `when` / `unless`; what follows is the
            // action and the guards
            let mut state_refs = Vec::new();
            for el in t.children_with_tokens() {
                match el {
                    rowan::NodeOrToken::Token(tok)
                        if matches!(tok.kind(), CONTEXT_KW | WHEN_KW) =>
                    {
                        break
                    }
                    rowan::NodeOrToken::Node(n) if n.kind() == NAME_REF => state_refs.push(n),
                    _ => {}
                }
            }
            for nr in state_refs {
                let Some(tok) = ident(&nr) else { continue };
                if states.contains(tok.text()) {
                    self.reference(file, tok.text_range(), Target::Variant(tok.text().into()));
                } else {
                    self.error(
                        file,
                        tok.text_range(),
                        format!("`{}` is not a state of `{}`", tok.text(), field.text()),
                    );
                }
            }
        }
    }

    fn check_type_path(&mut self, file: FileId, node: &SyntaxNode) {
        let Some(path) = ast::child(node, PATH) else {
            return;
        };
        let segs = ast::path_segments(&path);
        if segs.len() != 1 {
            return; // field paths in generic arguments (`order.currency`) or `old.Text`
        }
        let tok = &segs[0];
        let name = tok.text();
        if let Some(args) = node.parent().filter(|p| p.kind() == GENERIC_ARGS) {
            let generic = args
                .parent()
                .and_then(|g| ast::child(&g, PATH))
                .map(|p| ast::path_text(&p));
            if generic
                .as_deref()
                .map_or(false, |g| UNIT_GENERICS.contains(&g))
            {
                return; // unit or currency: checked by the type checker (phase 3)
            }
        }
        if let Some(payload) = ast::child(node, FIELD_LIST) {
            self.error(
                file,
                payload.text_range(),
                "variants with data are declared inside `enum(...)` or an `enum` declaration"
                    .into(),
            );
        }
        if BUILTIN_TYPES.contains(&name) {
            return;
        }
        match self.a.resolve(file, name) {
            Some(id) if self.a.decl(id).kind.is_type() => {
                self.reference(file, tok.text_range(), Target::Decl(id))
            }
            Some(id) => {
                let k = self.a.decl(id).kind.describe();
                self.error(
                    file,
                    tok.text_range(),
                    format!("`{name}` is a {k}, not a type"),
                );
            }
            None => self.unknown(file, tok, "type"),
        }
    }

    fn check_name_ref(&mut self, file: FileId, node: &SyntaxNode) {
        let Some(tok) = ident(node) else { return };
        let name = tok.text();
        if !is_capitalized(name) {
            return;
        }
        let Some(parent) = node.parent() else { return };
        match parent.kind() {
            PATH | IMPORT | NAME_LIST | TRANSITION | FLOW | REACTION | KEYWORD_STMT | AS_STMT
            | AS_EXPR => return,
            FIELD_EXPR if is_member_name(node) => return,
            CALL_EXPR if is_emit_callee(&parent) => return,
            _ => {}
        }
        if let Some(id) = self.a.resolve(file, name) {
            self.reference(file, tok.text_range(), Target::Decl(id));
        } else if BUILTIN_TYPES.contains(&name) || self.a.unit_codes.contains(name) {
        } else if self.a.variants.contains_key(name) {
            self.reference(file, tok.text_range(), Target::Variant(name.into()));
        } else {
            self.unknown(file, &tok, "name");
        }
    }
}

// ---- syntax helpers ---------------------------------------------------------------------

const TYPE_KINDS: &[quadra_syntax::SyntaxKind] = &[
    TYPE_PATH,
    TYPE_OPTIONAL,
    TYPE_UNION,
    TYPE_ENUM,
    TYPE_LIST,
    TYPE_SET,
    TYPE_PAREN,
    TYPE_TUPLE,
];

fn type_child(node: &SyntaxNode) -> Option<SyntaxNode> {
    node.children().find(|n| TYPE_KINDS.contains(&n.kind()))
}

/// A field with `= expr` (on its line or in its modifier block) is a formula.
fn is_formula(field: &SyntaxNode) -> bool {
    ast::child(field, VALUE).is_some()
        || ast::child(field, MODIFIER_BLOCK).map_or(false, |b| ast::child(&b, VALUE).is_some())
}

fn action_after_by(transition: &SyntaxNode) -> Option<SyntaxToken> {
    let mut after_by = false;
    for el in transition.children_with_tokens() {
        match el {
            rowan::NodeOrToken::Token(t) if t.kind() == CONTEXT_KW => after_by = t.text() == "by",
            rowan::NodeOrToken::Node(n) if n.kind() == NAME_REF && after_by => return ident(&n),
            _ => {}
        }
    }
    None
}

/// The NAME_REF after `.` / `?.` in a FIELD_EXPR.
fn is_member_name(node: &SyntaxNode) -> bool {
    let mut prev = node.prev_sibling_or_token();
    while let Some(el) = prev {
        match el.kind() {
            DOT | QUESTION_DOT => return true,
            WHITESPACE | COMMENT => prev = el.prev_sibling_or_token(),
            _ => return false,
        }
    }
    false
}

fn is_emit_callee(call: &SyntaxNode) -> bool {
    call.parent().map_or(false, |p| {
        p.kind() == KEYWORD_STMT && ast::ctx_keyword(&p).as_deref() == Some("emit")
    })
}

/// Subtrees that are deliberately wrong (`expect_compile_error`) or replace definitions
/// for a test (`patch`).
fn is_excluded(node: &SyntaxNode) -> bool {
    match node.kind() {
        EXPECT_BLOCK => ast::ctx_keyword(node).as_deref() == Some("expect_compile_error"),
        ENTITY => ast::ctx_keyword(node).as_deref() == Some("patch"),
        _ => false,
    }
}

/// Variant names of a member with an inline `enum(...)` type.
fn member_variants(member: &SyntaxNode) -> Vec<String> {
    let mut ty = match type_child(member) {
        Some(t) => t,
        None => return Vec::new(),
    };
    while ty.kind() == TYPE_OPTIONAL {
        match type_child(&ty) {
            Some(inner) => ty = inner,
            None => break,
        }
    }
    if ty.kind() != TYPE_ENUM {
        return Vec::new();
    }
    ast::children(&ty, VARIANT)
        .filter_map(|v| ast::name_text(&v))
        .collect()
}
