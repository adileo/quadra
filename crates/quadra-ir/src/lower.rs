//! Lowering: syntax tree + name analysis → IR.

use crate::*;
use quadra_sema::{Analysis, FileKind, Project};
use quadra_syntax::{ast, SyntaxKind, SyntaxKind::*, SyntaxNode, SyntaxToken};

pub fn lower(project: &Project, analysis: &Analysis) -> Model {
    let mut l = Lower {
        analysis,
        model: Model::default(),
        file: quadra_sema::FileId(0),
        pending: Vec::new(),
        patches: Vec::new(),
    };
    l.model.timezone = project.timezone.clone();
    l.model.manifest = project.manifest.clone();
    for f in project.files() {
        let i = f.id.0 as usize;
        if l.model.sources.len() <= i {
            l.model
                .sources
                .resize(i + 1, (String::new(), String::new()));
        }
        let rel = f.path.strip_prefix(&project.root).unwrap_or(&f.path);
        l.model.sources[i] = (rel.display().to_string(), f.text.clone());
    }
    for f in project.files() {
        // tests and migrations are scripts; their declarations are lowered like the others
        let _ = f.kind == FileKind::Script;
        l.file = f.id;
        // a `.qdp` file holds documents and labels, and they are written nowhere else
        let print = f.path.extension().is_some_and(|e| e == "qdp");
        for item in f.parse.syntax().children() {
            let k = item.kind();
            if print && !matches!(k, DOCUMENT | ATTACH | IMPORT | CONST | ERROR) {
                let span = l.span(&item);
                l.model.errors.push((span, "a `.qdp` file holds documents and labels (and their imports, constants and `attach`): this goes in a `.qd` file".to_string()));
                continue;
            }
            if !print && k == DOCUMENT {
                let span = l.span(&item);
                l.model.errors.push((span, "a document or a label is written in a `.qdp` file (`documents/invoice.qdp`), beside the code".to_string()));
            }
            l.item(&item);
        }
    }
    l.model.reindex();
    // the images of the documents: read from the project (where it is on a disk)
    let mut paths = Vec::new();
    fn images(items: &[PrintItem], out: &mut Vec<String>) {
        for it in items {
            match &it.kind {
                PrintKind::Element {
                    element,
                    value,
                    children,
                    ..
                } => {
                    if element == "image" {
                        if let Some(Expr {
                            kind: ExprKind::Lit(Lit::Str(p)),
                            ..
                        }) = value
                        {
                            out.push(p.clone());
                        }
                    }
                    images(children, out);
                }
                PrintKind::If {
                    then, otherwise, ..
                } => {
                    images(then, out);
                    images(otherwise, out);
                }
                PrintKind::For { body, .. } => images(body, out),
                PrintKind::Let { .. } => {}
            }
        }
    }
    for d in &l.model.documents {
        images(&d.items, &mut paths);
    }
    for p in paths {
        if p.starts_with('/') || p.contains("..") {
            continue;
        }
        if let Ok(bytes) = std::fs::read(project.root.join(&p)) {
            l.model.assets.insert(p, bytes);
        }
    }
    for ext in std::mem::take(&mut l.pending) {
        let Some(i) = l.model.entities.iter().position(|e| e.name == ext.name) else {
            continue;
        };
        let e = &mut l.model.entities[i];
        e.fields.extend(ext.fields);
        e.actions.extend(ext.actions);
        e.methods.extend(ext.methods);
        e.checks.extend(ext.checks);
        e.live_decls.extend(ext.live_decls);
        e.uniques.extend(ext.uniques);
        e.posts.extend(ext.posts);
        for flow in ext.flows {
            if e.flows.iter().any(|f| f.field == flow.field) {
                l.model
                    .errors
                    .push((flow.span, format!("`{}.{}` already has a flow: an `extend` can add a flow only to a field that has none (add the transitions to the existing `flow {}`)", e.name, flow.field, flow.field)));
            } else {
                e.flows.push(flow);
            }
        }
        if let Some(fr) = ext.frozen {
            if e.frozen.is_some() {
                l.model.errors.push((
                    fr.span,
                    format!("`{}` already has `frozen when`: an `extend` cannot change it; declare the fields it adds `live` (they follow the data), or list them in `except` where the entity is declared", e.name),
                ));
            } else {
                e.frozen = Some(fr);
            }
        }
    }
    // the field an `attach` fills is filled after the record is frozen: it is an exception
    for a in l.model.attaches.clone() {
        if let Some(e) = l.model.entities.iter_mut().find(|e| e.name == a.entity) {
            if let Some(fr) = e.frozen.as_mut() {
                if !fr.except.contains(&a.field) {
                    fr.except.push(a.field.clone());
                }
            }
        }
    }
    for e in &mut l.model.entities {
        if let Some(fr) = e.frozen.as_mut() {
            for (n, _) in &e.live_decls {
                if !fr.live.contains(n) {
                    fr.live.push(n.clone());
                }
            }
        }
    }
    for (target, fields) in std::mem::take(&mut l.patches) {
        if let Some(e) = l.model.entities.iter_mut().find(|e| e.name == target) {
            for f in fields {
                match e.fields.iter_mut().find(|x| x.name == f.name) {
                    Some(old) => *old = f,
                    None => e.fields.push(f),
                }
            }
        }
    }
    l.model.reindex();
    l.model
}

/// The members of an `extend`, merged into their entity once every file is lowered.
type Extension = Entity;

struct Lower<'a> {
    analysis: &'a Analysis,
    model: Model,
    file: quadra_sema::FileId,
    pending: Vec<Extension>,
    patches: Vec<(String, Vec<Field>)>,
}

const EXPR_KINDS: &[SyntaxKind] = &[
    NAME_REF,
    LITERAL,
    QUANTITY,
    BIN_EXPR,
    PREFIX_EXPR,
    IF_EXPR,
    FIELD_EXPR,
    CALL_EXPR,
    INDEX_EXPR,
    PAREN_EXPR,
    TUPLE_EXPR,
    LIST_EXPR,
    SET_EXPR,
    WHEN_EXPR,
    MATCH_EXPR,
    BLOCK_EXPR,
    AS_EXPR,
    FAIL_EXPR,
    LAMBDA_EXPR,
    ERROR,
];

const TYPE_KINDS: &[SyntaxKind] = &[
    TYPE_PATH,
    TYPE_OPTIONAL,
    TYPE_UNION,
    TYPE_ENUM,
    TYPE_LIST,
    TYPE_SET,
    TYPE_PAREN,
    TYPE_TUPLE,
];

fn is_expr(n: &SyntaxNode) -> bool {
    EXPR_KINDS.contains(&n.kind())
}

fn exprs(n: &SyntaxNode) -> Vec<SyntaxNode> {
    n.children().filter(is_expr).collect()
}

fn type_node(n: &SyntaxNode) -> Option<SyntaxNode> {
    n.children().find(|c| TYPE_KINDS.contains(&c.kind()))
}

fn tokens(n: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> {
    n.children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia())
}

fn ctx_words(n: &SyntaxNode) -> Vec<String> {
    tokens(n)
        .filter(|t| t.kind() == CONTEXT_KW)
        .map(|t| t.text().to_string())
        .collect()
}

/// The names a node refers to (`on price, stock`).
fn names_of_refs(n: &SyntaxNode) -> Vec<String> {
    ast::children(n, NAME_REF)
        .filter_map(|r| ast::token(&r, IDENT))
        .map(|t| t.text().to_string())
        .collect()
}

fn names(list: &SyntaxNode) -> Vec<String> {
    ast::children(list, NAME_REF)
        .filter_map(|n| ast::token(&n, IDENT))
        .map(|t| t.text().to_string())
        .collect()
}

fn path_segments(p: &SyntaxNode) -> Vec<String> {
    ast::path_segments(p)
        .iter()
        .map(|t| t.text().to_string())
        .collect()
}

fn unquote(s: &str) -> String {
    let inner = if s.starts_with("\"\"\"") && s.len() >= 6 {
        dedent(&s[3..s.len() - 3])
    } else if s.len() >= 2 {
        s[1..s.len() - 1].to_string()
    } else {
        String::new()
    };
    crate::unescape(&inner)
}

/// Remove the common indentation of a multi-line string and trim blank edges.
fn dedent(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let out: Vec<String> = lines
        .iter()
        .map(|l| {
            if l.len() >= indent {
                l[indent..].to_string()
            } else {
                l.trim().to_string()
            }
        })
        .collect();
    out.join("\n").trim_matches('\n').to_string()
}

impl<'a> Lower<'a> {
    /// The range of a node without the line breaks, blank lines, comments and dedents that
    /// end it: a diagnostic on a field or a declaration does not run over the next lines.
    fn span(&self, n: &SyntaxNode) -> Span {
        let significant = |t: &SyntaxToken| {
            !t.kind().is_trivia() && !matches!(t.kind(), NEWLINE | INDENT | DEDENT)
        };
        let tokens = || n.descendants_with_tokens().filter_map(|e| e.into_token());
        let range = match (
            tokens().find(|t| significant(t)),
            tokens().filter(|t| significant(t)).last(),
        ) {
            (Some(a), Some(b)) => TextRange::new(a.text_range().start(), b.text_range().end()),
            _ => n.text_range(),
        };
        Span::new(self.file, range)
    }

    // ---- items --------------------------------------------------------------------------

    fn item(&mut self, item: &SyntaxNode) {
        let span = self.span(item);
        match item.kind() {
            ENTITY => self.entity(item),
            LEDGER => {
                let name = ast::name_text(item).unwrap_or_default();
                let mut e = self.empty_entity(name, EntityKind::Entity, span);
                e.is_ledger = true;
                if let Some(list) = ast::child(item, MEMBER_LIST) {
                    self.members(&list, &mut e, false);
                }
                self.model.entities.push(e);
            }
            ENUM => {
                let name = ast::name_text(item).unwrap_or_default();
                let variants = item
                    .descendants()
                    .filter(|n| n.kind() == VARIANT)
                    .map(|v| self.variant(&v))
                    .collect();
                self.model.enums.push(EnumDef {
                    name,
                    variants,
                    span,
                });
            }
            FN => {
                let f = self.fn_def(item);
                self.model.fns.push(f);
            }
            CONST => {
                let name = ast::name_text(item).unwrap_or_default();
                let ty = type_node(item).map(|t| self.ty(&t));
                let value = ast::child(item, VALUE)
                    .map(|v| self.value(&v))
                    .unwrap_or(self.error_expr(item));
                self.model.consts.push(Const {
                    name,
                    ty,
                    value,
                    span,
                });
            }
            EVENT => {
                let name = ast::name_text(item).unwrap_or_default();
                let fields = ast::child(item, FIELD_LIST)
                    .map(|l| self.tuple_fields(&l))
                    .unwrap_or_default();
                self.model.events.push(Event { name, fields, span });
            }
            PUBLISH => self.publish(item, span),
            DOCUMENT => {
                let label = ctx_words(item).first().map(String::as_str) == Some("label");
                let name = ast::name_text(item).unwrap_or_default();
                let params = ast::child(item, PARAM_LIST)
                    .map(|p| self.params(&p))
                    .unwrap_or_default();
                let opts = self.print_opts(item);
                let items = self.print_items(item);
                self.model.documents.push(Document {
                    name,
                    label,
                    params,
                    opts,
                    items,
                    span,
                });
            }
            ATTACH => {
                let document = names_of_refs(item).into_iter().next().unwrap_or_default();
                let path = ast::child(item, PATH)
                    .map(|p| ast::path_text(&p))
                    .unwrap_or_default();
                let (entity, field) = path.split_once('.').unwrap_or((path.as_str(), ""));
                // the condition: the expression after `when` (the document's name is a NAME_REF)
                let mut after_when = false;
                let mut when_node = None;
                for c in item.children_with_tokens() {
                    match c {
                        rowan::NodeOrToken::Token(t) if t.kind() == WHEN_KW => after_when = true,
                        rowan::NodeOrToken::Node(n) if after_when && is_expr(&n) => {
                            when_node = Some(n);
                            break;
                        }
                        _ => {}
                    }
                }
                let when = when_node.map(|e| self.expr(&e));
                let signer = item
                    .children_with_tokens()
                    .filter_map(|t| t.into_token())
                    .find(|t| t.kind() == STRING)
                    .map(|t| unquote(t.text()));
                self.model.attaches.push(Attach {
                    document,
                    entity: entity.to_string(),
                    field: field.to_string(),
                    signer,
                    when,
                    span,
                });
            }
            ENDPOINT => {
                let mut names =
                    ast::children(item, NAME).map(|n| n.text().to_string().trim().to_string());
                let method = names.next().unwrap_or_default().to_uppercase();
                let connection = names.next().unwrap_or_default();
                let binding = names.next().unwrap_or_default();
                let path = item
                    .children_with_tokens()
                    .filter_map(|t| t.into_token())
                    .find(|t| t.kind() == STRING)
                    .map(|t| unquote(t.text()))
                    .unwrap_or_default();
                let mut types = ast::children(item, PATH).map(|p| ast::path_text(&p));
                let query = types.next().unwrap_or_default();
                let answer = types.next().unwrap_or_default();
                let key = ast::children(item, OUTBOUND_CLAUSE)
                    .find(|c| ctx_words(c).first().map(String::as_str) == Some("key"))
                    .and_then(|c| exprs(&c).first().map(|x| self.expr(x)));
                let body = ast::child(item, BLOCK_EXPR)
                    .map(|b| self.expr(&b))
                    .unwrap_or_else(|| self.error_expr(item));
                self.model.endpoints.push(Endpoint {
                    method,
                    path,
                    connection,
                    binding,
                    query,
                    answer,
                    key,
                    body,
                    span,
                });
            }
            INBOUND => {
                let mut names =
                    ast::children(item, NAME).map(|n| n.text().to_string().trim().to_string());
                let name = names.next().unwrap_or_default();
                let connection = names.next().unwrap_or_default();
                let binding = names.next().unwrap_or_default();
                let path = item
                    .children_with_tokens()
                    .filter_map(|t| t.into_token())
                    .find(|t| t.kind() == STRING)
                    .map(|t| unquote(t.text()));
                let ty = ast::child(item, PATH)
                    .map(|p| ast::path_text(&p))
                    .unwrap_or_default();
                let clause = |w: &str| {
                    ast::children(item, OUTBOUND_CLAUSE)
                        .find(|c| ctx_words(c).first().map(String::as_str) == Some(w))
                };
                let key = clause("key").and_then(|c| exprs(&c).first().map(|x| self.expr(x)));
                let pull = clause("pull").map(|c| {
                    let path = c
                        .children_with_tokens()
                        .filter_map(|t| t.into_token())
                        .find(|t| t.kind() == STRING)
                        .map(|t| unquote(t.text()))
                        .unwrap_or_default();
                    let every = exprs(&c)
                        .first()
                        .map(|x| self.expr(x))
                        .unwrap_or_else(|| self.error_expr(&c));
                    let items = clause("items")
                        .and_then(|c| ast::child(&c, PATH))
                        .map(|p| ast::path_text(&p))
                        .and_then(|t| {
                            t.split_once('.')
                                .map(|(a, b)| (a.to_string(), b.to_string()))
                        });
                    let cursor =
                        clause("cursor").and_then(|c| exprs(&c).first().map(|x| self.expr(x)));
                    Pull {
                        path,
                        every,
                        items,
                        cursor,
                    }
                });
                let retry_for = ast::children(item, OUTBOUND_CLAUSE)
                    .find(|c| {
                        ctx_words(c).first().map(String::as_str) == Some("retry")
                            && ast::token(c, FOR_KW).is_some()
                    })
                    .and_then(|c| exprs(&c).first().map(|x| self.expr(x)));
                let backoff =
                    clause("backoff").and_then(|c| exprs(&c).first().map(|x| self.expr(x)));
                let ordered =
                    clause("ordered").and_then(|c| exprs(&c).first().map(|x| self.expr(x)));
                let latest = clause("latest").and_then(|c| {
                    let es = exprs(&c);
                    match (es.first(), es.get(1)) {
                        (Some(k), Some(u)) => Some((self.expr(k), self.expr(u))),
                        _ => None,
                    }
                });
                let atomic = clause("atomic").map(|c| {
                    if ctx_words(&c).get(2).map(String::as_str) == Some("row") {
                        Atomic::Row
                    } else {
                        Atomic::File
                    }
                });
                let body = ast::child(item, BLOCK)
                    .map(|b| self.stmts(&b))
                    .unwrap_or_default();
                self.model.inbounds.push(Inbound {
                    name,
                    connection,
                    path,
                    binding,
                    ty,
                    key,
                    pull,
                    retry_for,
                    backoff,
                    ordered,
                    latest,
                    atomic,
                    body,
                    span,
                });
            }
            OUTBOUND => {
                let mut names =
                    ast::children(item, NAME).map(|n| n.text().to_string().trim().to_string());
                let event = names.next().unwrap_or_default();
                let connection = names.next().unwrap_or_default();
                let path = item
                    .children_with_tokens()
                    .filter_map(|t| t.into_token())
                    .find(|t| t.kind() == STRING)
                    .map(|t| unquote(t.text()));
                // `as e:`: the binding is the name after the keyword
                let binding = ast::token(item, AS_KW).and_then(|_| {
                    ast::children(item, NAME)
                        .nth(2)
                        .map(|n| n.text().to_string().trim().to_string())
                });
                let body = ast::child(item, BLOCK_EXPR).map(|b| self.expr(&b));
                let (reply, policy) = self.outbound_policy(item);
                let batch = ast::children(item, OUTBOUND_CLAUSE)
                    .find(|c| ctx_words(c).first().map(String::as_str) == Some("batch"))
                    .map(|c| {
                        let n = tokens(&c)
                            .find(|t| t.kind() == NUMBER)
                            .or_else(|| {
                                ast::child(&c, LITERAL)
                                    .and_then(|l| tokens(&l).find(|t| t.kind() == NUMBER))
                            })
                            .and_then(|t| t.text().parse::<u32>().ok())
                            .unwrap_or(0);
                        let within = exprs(&c)
                            .into_iter()
                            .find(|x| x.kind() != LITERAL)
                            .map(|x| self.expr(&x));
                        (n, within)
                    });
                // a block of policy lines only: the request stays the default one
                let body = body.filter(|b| !matches!(&b.kind, ExprKind::Block(s) if s.is_empty()));
                self.model.outbounds.push(Outbound {
                    event,
                    connection,
                    path,
                    binding,
                    body,
                    reply,
                    policy,
                    batch,
                    publishes: None,
                    span,
                });
            }
            REACTION => self.reaction(item),
            ROLE => {
                let name = ast::name_text(item).unwrap_or_default();
                let parent = ast::child(item, IS_CLAUSE)
                    .and_then(|c| ast::child(&c, PATH))
                    .map(|p| ast::path_text(&p));
                let rules = ast::children(item, RULE)
                    .map(|r| self.rule(&r, false))
                    .collect();
                self.model.roles.push(Role {
                    name,
                    parent,
                    rules,
                    span,
                });
            }
            RULE => {
                let r = self.rule(item, true);
                self.model.rules.push(r);
            }
            SEED => {
                let body = self.block_of(item);
                self.model.seeds.push(Block {
                    name: String::new(),
                    extends: None,
                    body,
                    span,
                });
            }
            FIXTURE => {
                let name = ast::name_text(item).unwrap_or_default();
                let extends = ast::child(item, PATH).map(|p| ast::path_text(&p));
                let body = self.block_of(item);
                self.model.fixtures.push(Block {
                    name,
                    extends,
                    body,
                    span,
                });
            }
            TEST => {
                let name = tokens(item)
                    .find(|t| t.kind() == STRING)
                    .map(|t| unquote(t.text()))
                    .unwrap_or_default();
                let body = self.block_of(item);
                self.model.tests.push(Block {
                    name,
                    extends: None,
                    body,
                    span,
                });
            }
            METRIC | CONTROL => {
                let name = ast::name_text(item).unwrap_or_default();
                let params = ast::child(item, PARAM_LIST)
                    .map(|p| self.params(&p))
                    .unwrap_or_default();
                let ret = ast::child(item, RET_TYPE)
                    .and_then(|r| type_node(&r))
                    .map(|t| self.ty(&t));
                let mut meta = Vec::new();
                let mut formulas = Vec::new();
                if let Some(list) = ast::child(item, MEMBER_LIST) {
                    for m in list.children() {
                        let key = ast::name_text(&m).unwrap_or_default();
                        match m.kind() {
                            META => {
                                let v = exprs(&m)
                                    .first()
                                    .map(|e| self.expr(e))
                                    .unwrap_or(self.error_expr(&m));
                                meta.push((key, v));
                            }
                            FIELD => {
                                if let Some(v) = ast::child(&m, VALUE) {
                                    let e = self.value(&v);
                                    formulas.push((key, e));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                self.model.metrics.push(Metric {
                    name,
                    params,
                    ret,
                    meta,
                    formulas,
                    control: item.kind() == CONTROL,
                    span,
                });
            }
            MIGRATION | CHANGE => {
                let name = ast::name_text(item).unwrap_or_default();
                let body = self.block_of(item);
                let b = Block {
                    name,
                    extends: None,
                    body,
                    span,
                };
                if item.kind() == MIGRATION {
                    self.model.migrations.push(b);
                } else {
                    self.model.changes.push(b);
                }
            }
            _ => {}
        }
    }

    fn empty_entity(&self, name: String, kind: EntityKind, span: Span) -> Entity {
        Entity {
            name,
            kind,
            is_ledger: false,
            temporary: false,
            load_from: None,
            format: None,
            format_options: Vec::new(),
            namespaces: Vec::new(),
            traits: Vec::new(),
            fields: Vec::new(),
            actions: Vec::new(),
            methods: Vec::new(),
            checks: Vec::new(),
            uniques: Vec::new(),
            frozen: None,
            live_decls: Vec::new(),
            flows: Vec::new(),
            posts: Vec::new(),
            retention: Retention::default(),
            history: None,
            span,
        }
    }

    fn entity(&mut self, item: &SyntaxNode) {
        let words = ctx_words(item);
        if words.iter().any(|w| w == "patch") {
            // tests only: replace definitions of an existing entity
            let Some(path) = ast::child(item, PATH) else {
                return;
            };
            let target = path_segments(&path).into_iter().next().unwrap_or_default();
            let mut tmp = self.empty_entity(target.clone(), EntityKind::Entity, self.span(item));
            if let Some(list) = ast::child(item, MEMBER_LIST) {
                self.members(&list, &mut tmp, true);
            }
            self.patches.push((target, tmp.fields));
            return;
        }
        let span = self.span(item);
        if ast::token(item, EXTEND_KW).is_some() {
            let Some(path) = ast::child(item, PATH) else {
                return;
            };
            let seg = path_segments(&path).into_iter().next().unwrap_or_default();
            let target = self
                .analysis
                .resolve(self.file, &seg)
                .map(|d| self.analysis.decl(d).name.clone())
                .unwrap_or(seg);
            let mut tmp = self.empty_entity(target.clone(), EntityKind::Entity, span);
            if let Some(list) = ast::child(item, MEMBER_LIST) {
                self.members(&list, &mut tmp, true);
            }
            self.pending.push(tmp);
            return;
        }
        let kind = if ast::token(item, RECORD_KW).is_some() {
            EntityKind::Record
        } else if ast::token(item, TRAIT_KW).is_some() {
            EntityKind::Trait
        } else {
            EntityKind::Entity
        };
        let name = ast::name_text(item).unwrap_or_default();
        let mut e = self.empty_entity(name, kind, span);
        e.temporary = words.iter().any(|w| w == "temporary");
        if let Some(c) = ast::child(item, FORMAT_CLAUSE) {
            e.format = ast::name_text(&c);
            e.format_options = ast::children(&c, FORMAT_OPTION)
                .map(|o| self.format_option(&o))
                .collect();
        }
        if let Some(clause) = ast::child(item, IS_CLAUSE) {
            e.traits = ast::children(&clause, PATH)
                .map(|p| ast::path_text(&p))
                .collect();
        }
        if let Some(list) = ast::child(item, MEMBER_LIST) {
            self.members(&list, &mut e, false);
        }
        if e.load_from.is_some() && !e.temporary {
            let at = ast::child(item, MEMBER_LIST)
                .and_then(|l| l.children().find(|m| m.kind() == LOAD))
                .map(|m| self.span(&m))
                .unwrap_or(span);
            self.model.errors.push((at, format!("`load from` is only for a `temporary entity` (used by migrations): `{}` is not temporary", e.name)));
        }
        self.model.entities.push(e);
    }

    fn members(&mut self, list: &SyntaxNode, e: &mut Entity, extension: bool) {
        for m in list.children() {
            let span = self.span(&m);
            match m.kind() {
                FIELD => {
                    let f = self.field(&m, extension);
                    e.fields.push(f);
                }
                CHECK | WARN => {
                    let mut per = Vec::new();
                    if let Some(list) = ast::child(&m, NAME_LIST) {
                        per = names(&list);
                    }
                    let es = exprs(&m);
                    let cond = es
                        .first()
                        .map(|x| self.expr(x))
                        .unwrap_or(self.error_expr(&m));
                    let message = es
                        .get(1)
                        .map(|x| self.expr(x))
                        .unwrap_or(self.error_expr(&m));
                    e.checks.push(Check {
                        per,
                        cond,
                        message,
                        warn: m.kind() == WARN,
                        span,
                    });
                }
                NAMESPACE => e.namespaces.push(Namespace {
                    prefix: ast::name_text(&m),
                    uri: tokens(&m)
                        .find(|t| t.kind() == STRING)
                        .map(|t| unquote(t.text()))
                        .unwrap_or_default(),
                    span,
                }),
                LOAD if extension => {
                    self.model.errors.push((span, "`load from` cannot be used in an `extend`: it belongs in the declaration of a `temporary entity`".to_string()));
                }
                LOAD => {
                    e.load_from = tokens(&m)
                        .find(|t| t.kind() == STRING)
                        .map(|t| unquote(t.text()));
                }
                UNIQUE => {
                    let fields = ast::child(&m, NAME_LIST)
                        .map(|l| names(&l))
                        .unwrap_or_default();
                    let when = exprs(&m).first().map(|x| self.expr(x));
                    e.uniques.push(Unique { fields, when, span });
                }
                RETENTION => {
                    let mut r = std::mem::take(&mut e.retention);
                    self.retention(&m, &mut r);
                    e.retention = r;
                }
                HISTORY => {
                    e.history = Some(self.term(&m, None));
                }
                FROZEN => {
                    let when = exprs(&m)
                        .first()
                        .map(|x| self.expr(x))
                        .unwrap_or(self.error_expr(&m));
                    let mut except = Vec::new();
                    let mut live = Vec::new();
                    let mut current = "";
                    for el in m.children_with_tokens() {
                        match el {
                            rowan::NodeOrToken::Token(t) if t.kind() == CONTEXT_KW => {
                                current = if t.text() == "except" {
                                    "except"
                                } else if t.text() == "live" {
                                    "live"
                                } else {
                                    ""
                                }
                            }
                            rowan::NodeOrToken::Node(n) if n.kind() == NAME_LIST => {
                                if current == "except" {
                                    except.extend(names(&n));
                                } else if current == "live" {
                                    live.extend(names(&n));
                                }
                            }
                            _ => {}
                        }
                    }
                    e.frozen = Some(Frozen {
                        when,
                        except,
                        live,
                        span,
                    });
                }
                LIVE => {
                    let span = self.span(&m);
                    for n in ast::child(&m, NAME_LIST)
                        .map(|l| names(&l))
                        .unwrap_or_default()
                    {
                        e.live_decls.push((n, span));
                    }
                }
                FLOW => {
                    let field = ast::child(&m, NAME_REF)
                        .and_then(|n| ast::token(&n, IDENT))
                        .map(|t| t.text().to_string())
                        .unwrap_or_default();
                    let transitions = ast::children(&m, TRANSITION)
                        .map(|t| self.transition(&t))
                        .collect();
                    e.flows.push(Flow {
                        field,
                        transitions,
                        span,
                    });
                }
                ACTION => {
                    let name = ast::name_text(&m).unwrap_or_default();
                    let params = ast::child(&m, PARAM_LIST)
                        .map(|p| self.params(&p))
                        .unwrap_or_default();
                    let ret = ast::child(&m, RET_TYPE)
                        .and_then(|r| type_node(&r))
                        .map(|t| self.ty(&t));
                    let body = self.block_of(&m);
                    e.actions.push(Action {
                        name,
                        params,
                        ret,
                        body,
                        span,
                    });
                }
                FN => {
                    let f = self.fn_def(&m);
                    e.methods.push(f);
                }
                POST => {
                    let ledger = ast::child(&m, PATH)
                        .map(|p| ast::path_text(&p))
                        .unwrap_or_default();
                    let when = exprs(&m).first().map(|x| self.expr(x));
                    let body = self.block_of(&m);
                    e.posts.push(Post {
                        ledger,
                        when,
                        body,
                        span,
                    });
                }
                _ => {}
            }
        }
    }

    fn field(&mut self, m: &SyntaxNode, extension: bool) -> Field {
        let name = ast::name_text(m).unwrap_or_default();
        let ledger_kw = tokens(m)
            .find(|t| t.kind() == CONTEXT_KW)
            .map(|t| t.text().to_string());
        let ty = type_node(m).map(|t| self.ty(&t));
        let file = type_node(m).and_then(|t| self.file_spec(&t));
        let mut f = Field {
            name,
            ty,
            kind: FieldKind::Stored,
            default: None,
            formula: None,
            where_: None,
            required_when: None,
            unique: false,
            parent: false,
            via: None,
            through: None,
            per_company: false,
            at: None,
            pattern: None,
            decimals: None,
            file,
            extension,
            default_override: false,
            retention: Retention::default(),
            personal: false,
            span: self.span(m),
        };
        let mut parts: Vec<SyntaxNode> = m.children().collect();
        if let Some(block) = ast::child(m, MODIFIER_BLOCK) {
            parts.extend(block.children());
        }
        for p in parts {
            match p.kind() {
                MODIFIER => self.modifier(&p, &mut f),
                VALUE => f.formula = Some(self.value(&p)),
                DEFAULT_VALUE => f.default = Some(self.value(&p)),
                _ => {}
            }
        }
        f.kind = match ledger_kw.as_deref() {
            Some("key") => FieldKind::LedgerKey,
            Some("measure") => FieldKind::LedgerMeasure,
            Some("attr") => FieldKind::LedgerAttr,
            _ if f.formula.is_some() => FieldKind::Formula,
            _ => FieldKind::Stored,
        };
        f.default_override = f.ty.is_none() && f.formula.is_none() && f.default.is_some();
        f
    }

    /// `expire 6 months after last_contact`, `retain 10 years after date[, then expire]`.
    fn retention(&mut self, m: &SyntaxNode, r: &mut Retention) {
        let kw = tokens(m)
            .next()
            .map(|t| t.text().to_string())
            .unwrap_or_default();
        let after = ast::child(m, PATH).map(|p| ast::path_text(&p));
        let t = self.term(m, after);
        let then_expire = tokens(m).any(|t| t.text() == "then");
        if kw == "retain" {
            if then_expire {
                r.expire = Some(t.clone());
            }
            r.retain = Some(t);
        } else {
            r.expire = Some(t);
        }
    }

    /// The number and unit of a retention or `history` line, and the field after which.
    fn term(&mut self, m: &SyntaxNode, after: Option<String>) -> Term {
        let n = tokens(m)
            .find(|t| t.kind() == NUMBER)
            .and_then(|t| t.text().replace('_', "").parse().ok())
            .unwrap_or(0);
        let unit = tokens(m)
            .skip_while(|t| t.kind() != NUMBER)
            .find(|t| t.kind() == IDENT || t.kind() == CONTEXT_KW)
            .map(|t| t.text().to_string())
            .unwrap_or_default();
        Term {
            n,
            unit,
            after: after.unwrap_or_default(),
            span: self.span(m),
        }
    }

    fn modifier(&mut self, m: &SyntaxNode, f: &mut Field) {
        let first = tokens(m).next();
        let Some(first) = first else { return };
        match (first.kind(), first.text()) {
            (WHERE_KW, _) => {
                let op = tokens(m).nth(1).and_then(|t| match t.kind() {
                    GT => Some(BinOp::Gt),
                    GE => Some(BinOp::Ge),
                    LT => Some(BinOp::Lt),
                    LE => Some(BinOp::Le),
                    EQ2 => Some(BinOp::Eq),
                    NEQ => Some(BinOp::Ne),
                    _ => None,
                });
                let e = exprs(m)
                    .first()
                    .map(|x| self.expr(x))
                    .unwrap_or(self.error_expr(m));
                f.where_ = Some(match op {
                    Some(op) => Where::Compare(op, e),
                    None => Where::Expr(e),
                });
            }
            (_, "required") => f.required_when = exprs(m).first().map(|x| self.expr(x)),
            (_, "unique") => f.unique = true,
            (_, "personal") => f.personal = true,
            (_, "expire" | "retain") => self.retention(m, &mut f.retention),
            (_, "parent") => f.parent = true,
            (_, "per") => f.per_company = true,
            (_, "via") => f.via = ast::child(m, PATH).map(|p| ast::path_text(&p)),
            (_, "through") => f.through = ast::child(m, PATH).map(|p| ast::path_text(&p)),
            (_, "at") => {
                f.at = tokens(m)
                    .find(|t| t.kind() == STRING)
                    .map(|t| unquote(t.text()))
            }
            (_, "format") => {
                f.pattern = tokens(m)
                    .find(|t| t.kind() == STRING)
                    .map(|t| unquote(t.text()))
            }
            // a number of an EBCDIC file: packed (COMP-3) or zoned, kept as its `format`
            (_, w @ ("packed" | "zoned")) => f.pattern = Some(w.to_string()),
            (_, "max") => {
                // `max 10 MB`: a size that cannot be read is 0, refused by the checker
                let number = tokens(m).find(|t| t.kind() == NUMBER);
                let unit = tokens(m).filter(|t| t.kind() == CONTEXT_KW).nth(1);
                let bytes = match (number, unit) {
                    (Some(n), Some(u)) => files::size_bytes(n.text(), u.text()).unwrap_or(0),
                    _ => 0,
                };
                f.file.get_or_insert_with(Default::default).max = Some((bytes, self.span(m)));
            }
            (_, "decimals") => {
                // a number too large for one is refused as `u32::MAX` by the checker
                f.decimals = tokens(m)
                    .find(|t| t.kind() == NUMBER)
                    .map(|t| t.text().replace('_', "").parse().unwrap_or(u32::MAX))
            }
            _ => {}
        }
    }

    /// `sep=";"` in `from csv(sep=";")`: its value is a literal (anything else is refused by
    /// the checker).
    fn format_option(&mut self, o: &SyntaxNode) -> FormatOption {
        let name = ast::name_text(o).unwrap_or_default();
        let value = match exprs(o).first().map(|x| self.expr(x)).map(|x| x.kind) {
            Some(ExprKind::Lit(Lit::Str(t))) => OptionValue::Text(t),
            Some(ExprKind::Lit(Lit::Bool(b))) => OptionValue::Bool(b),
            Some(ExprKind::Lit(Lit::Number(n))) => OptionValue::Number(n),
            _ => OptionValue::Other,
        };
        FormatOption {
            name,
            value,
            span: self.span(o),
        }
    }

    fn transition(&mut self, t: &SyntaxNode) -> Transition {
        let mut from = Vec::new();
        let mut to = String::new();
        let mut by = None;
        let mut when = None;
        let mut unless = None;
        let mut state = 0; // 0 = from, 1 = to, 2 = by, 3 = when, 4 = unless
        for el in t.children_with_tokens() {
            match el {
                rowan::NodeOrToken::Token(tok) => match tok.kind() {
                    ARROW => state = 1,
                    WHEN_KW => state = 3,
                    CONTEXT_KW if tok.text() == "by" => state = 2,
                    CONTEXT_KW if tok.text() == "unless" => state = 4,
                    _ => {}
                },
                rowan::NodeOrToken::Node(n) => {
                    let text = || {
                        ast::token(&n, IDENT)
                            .map(|t| t.text().to_string())
                            .unwrap_or_default()
                    };
                    match state {
                        0 if n.kind() == NAME_REF => from.push(text()),
                        1 if n.kind() == NAME_REF => to = text(),
                        2 if n.kind() == NAME_REF => by = Some(text()),
                        3 if is_expr(&n) => when = Some(self.expr(&n)),
                        4 if is_expr(&n) => unless = Some(self.expr(&n)),
                        _ => {}
                    }
                }
            }
        }
        Transition {
            from,
            to,
            by,
            when,
            unless,
            span: self.span(t),
        }
    }

    /// The `reply` and the lines of policy of an `outbound` or a `publish`.
    fn outbound_policy(&mut self, item: &SyntaxNode) -> (Option<String>, Policy) {
        let reply = ast::children(item, OUTBOUND_CLAUSE)
            .find(|c| {
                c.children_with_tokens()
                    .filter_map(|t| t.into_token())
                    .any(|t| t.text() == "reply")
            })
            .and_then(|c| ast::child(&c, PATH))
            .map(|p| ast::path_text(&p));
        let mut policy = Policy::default();
        for c in ast::children(item, OUTBOUND_CLAUSE) {
            let mut words = ctx_words(&c);
            // `retry for`: `for` is a keyword, not a contextual word
            if ast::token(&c, FOR_KW).is_some() {
                words.insert(1, "for".into());
            }
            let value = exprs(&c).first().map(|x| self.expr(x));
            let slot = match (
                words.first().map(String::as_str),
                words.get(1).map(String::as_str),
            ) {
                (Some("retry"), Some("for")) => &mut policy.retry_for,
                (Some("retry"), _) => &mut policy.retry_times,
                (Some("backoff"), _) => &mut policy.backoff,
                (Some("expires"), _) => &mut policy.expires,
                (Some("unless"), _) => &mut policy.unless,
                (Some("delay"), _) => &mut policy.delay,
                (Some("window"), _) => &mut policy.window,
                (Some("key"), _) => &mut policy.key,
                (Some("ordered"), _) => &mut policy.ordered,
                (Some("latest"), _) => &mut policy.latest,
                _ => continue,
            };
            *slot = value;
        }
        (reply, policy)
    }

    /// `publish Product to shop as p:` is sugar for what it means: an event per record
    /// (`PublishProductToShop(record: Product)`), emitted when a record is created or one of
    /// the fields of `on` changes and `when` holds, unless the change came from an inbound of
    /// the same connection (no echo); and an outbound of it to the connection, `latest by` the
    /// record and `delay` the debounce, whose block reads the record as it was at the emit.
    fn publish(&mut self, item: &SyntaxNode, span: Span) {
        let entity = ast::child(item, PATH)
            .map(|p| ast::path_text(&p))
            .unwrap_or_default();
        let mut names = ast::children(item, NAME).map(|n| n.text().to_string().trim().to_string());
        let connection = names.next().unwrap_or_default();
        let binding = names.next().unwrap_or_default();
        let clause = |w: &str| {
            ast::children(item, OUTBOUND_CLAUSE).find(|c| {
                c.children_with_tokens()
                    .filter_map(|t| t.into_token())
                    .any(|t| t.text() == w)
            })
        };
        let when = clause("when").and_then(|c| exprs(&c).first().map(|x| self.expr(x)));
        let on: Vec<String> = clause("on").map(|c| names_of_refs(&c)).unwrap_or_default();
        let debounce = clause("debounce").and_then(|c| exprs(&c).first().map(|x| self.expr(x)));
        let block = ast::child(item, BLOCK_EXPR)
            .map(|b| self.expr(&b))
            .unwrap_or_else(|| self.error_expr(item));
        let e = |kind: ExprKind| Expr { kind, span };
        let name = |n: &str| e(ExprKind::Name(n.to_string()));
        let call = |f: &str, args: Vec<Expr>| {
            e(ExprKind::Call {
                callee: Box::new(name(f)),
                args: args
                    .into_iter()
                    .map(|value| Arg {
                        name: None,
                        value,
                        spread: false,
                    })
                    .collect(),
            })
        };
        let text = |s: &str| e(ExprKind::Lit(Lit::Str(s.to_string())));
        let and = |a: Expr, b: Expr| {
            e(ExprKind::Binary {
                op: BinOp::And,
                lhs: Box::new(a),
                rhs: Box::new(b),
            })
        };
        let event = ast::publish_event(item).map(|(n, _)| n).unwrap_or_default();
        self.model.events.push(Event {
            name: event.clone(),
            fields: vec![(
                "record".into(),
                Ty::Path {
                    segments: vec![entity.clone()],
                    args: vec![],
                    span,
                },
            )],
            span,
        });
        // on created and on changes: emit, unless the change came from the same connection
        for created in [true, false] {
            let mut cond = e(ExprKind::Unary {
                op: UnOp::Not,
                expr: Box::new(call("__from", vec![text(&connection)])),
            });
            if let Some(w) = &when {
                cond = and(w.clone(), cond);
            }
            if !created && !on.is_empty() {
                let mut args = vec![name(&binding)];
                args.extend(on.iter().map(|f| text(f)));
                cond = and(cond, call("__changed", args));
            }
            let emit = e(ExprKind::Call {
                callee: Box::new(name(&event)),
                args: vec![Arg {
                    name: Some("record".into()),
                    value: name(&binding),
                    spread: false,
                }],
            });
            self.model.reactions.push(Reaction {
                entity: entity.clone(),
                trigger: if created {
                    Trigger::Created
                } else {
                    Trigger::Changes
                },
                binding: binding.clone(),
                converges: false,
                body: vec![Stmt {
                    kind: StmtKind::If {
                        binding: None,
                        cond,
                        then: vec![Stmt {
                            kind: StmtKind::Emit(emit),
                            span,
                        }],
                        otherwise: None,
                    },
                    span,
                }],
                span,
            });
        }
        // the outbound, with `p` the record besides the event
        let (reply, mut policy) = self.outbound_policy(item);
        if policy.latest.is_none() {
            policy.latest = Some(name(&binding));
        }
        if debounce.is_some() {
            policy.delay = debounce;
        }
        let body = Some(block);
        self.model.outbounds.push(Outbound {
            event,
            connection,
            path: None,
            binding: Some("__published".into()),
            body,
            reply,
            policy,
            batch: None,
            publishes: Some(binding),
            span,
        });
    }

    fn reaction(&mut self, item: &SyntaxNode) {
        let words = ctx_words(item);
        let path = ast::child(item, PATH)
            .map(|p| path_segments(&p))
            .unwrap_or_default();
        let entity = path.first().cloned().unwrap_or_default();
        let trigger = if words.first().map(String::as_str) == Some("before") {
            Trigger::Before(path.get(1).cloned().unwrap_or_default())
        } else if words.iter().any(|w| w == "enters") {
            let state = ast::child(item, NAME_REF)
                .and_then(|n| ast::token(&n, IDENT))
                .map(|t| t.text().to_string())
                .unwrap_or_default();
            Trigger::Enters(state)
        } else if words.iter().any(|w| w == "becomes") {
            Trigger::Becomes(
                exprs(item)
                    .first()
                    .map(|x| self.expr(x))
                    .unwrap_or(self.error_expr(item)),
            )
        } else if words.iter().any(|w| w == "created") {
            Trigger::Created
        } else if words.iter().any(|w| w == "changes") {
            Trigger::Changes
        } else if words.iter().any(|w| w == "delivered") {
            Trigger::Delivered
        } else if words.iter().any(|w| w == "failed") {
            Trigger::Failed
        } else if words.iter().any(|w| w == "expired") {
            Trigger::Expired
        } else if words.iter().any(|w| w == "skipped") {
            Trigger::Skipped
        } else if words.iter().any(|w| w == "rejected") {
            Trigger::Rejected
        } else {
            if words.iter().any(|w| w == "deleted") {
                self.model.errors.push((
                    self.span(item),
                    format!(
                        "records are not deleted in Quadra, so `on {entity} deleted` would never run: a record that should no longer count is cancelled or deactivated; react to that with `on {entity} enters <state>` or `on {entity} becomes <condition>`"
                    ),
                ));
            }
            // a syntax error was already reported for any other word
            return;
        };
        let binding = ast::name_text(item).unwrap_or_default();
        let converges = words.iter().any(|w| w == "converges");
        let body = self.block_of(item);
        self.model.reactions.push(Reaction {
            entity,
            trigger,
            binding,
            converges,
            body,
            span: self.span(item),
        });
    }

    /// A permission rule. `effect` is `allow`, `deny`, `hide` or `show`; `verbs` never contain
    /// `hide` / `show`.
    fn rule(&mut self, r: &SyntaxNode, top_level: bool) -> Rule {
        let span = self.span(r);
        let mut word: Option<String> = None;
        let mut verbs: Vec<String> = Vec::new();
        let mut history = false;
        let mut targets = Vec::new();
        for el in r.children_with_tokens() {
            match el {
                rowan::NodeOrToken::Token(t) if t.kind() == CONTEXT_KW => match t.text() {
                    "deny" | "allow" => word = Some(t.text().to_string()),
                    "history" => history = true,
                    v => verbs.push(v.to_string()),
                },
                rowan::NodeOrToken::Token(t) if t.kind() == STAR => {
                    targets.push(vec!["*".to_string()])
                }
                rowan::NodeOrToken::Node(n) if n.kind() == PATH => targets.push(path_segments(&n)),
                _ => {}
            }
        }
        let mut effect = word.clone().unwrap_or_else(|| "allow".to_string());
        if verbs.iter().any(|v| v == "delete") {
            self.model.errors.push((
                span,
                "records are not deleted in Quadra, so there is no `delete` permission: a record that should no longer count is cancelled, reversed or deactivated by an action (`run Entity.cancel`)".into(),
            ));
        }
        if verbs.iter().any(|v| v == "derived") {
            self.model.errors.push((span, "`derived` is not a permission: use `read`, `create`, `edit`, `run`, `erase`, `all`, `hide` or `show`".into()));
        }
        if let Some(vis) = verbs.iter().find(|v| *v == "hide" || *v == "show").cloned() {
            if verbs.len() > 1 {
                self.model.errors.push((span, format!("`{vis}` cannot be combined with other permissions: write it on its own line (`{vis} Entity.field`)")));
            } else if top_level {
                self.model.errors.push((
                    span,
                    format!(
                        "`{vis}` is written inside a `role`: it applies to the users of that role"
                    ),
                ));
            } else if let Some(w) = &word {
                self.model.errors.push((
                    span,
                    format!("`{w} {vis}` has no meaning: write `{vis} Entity.field`"),
                ));
            }
            effect = vis;
            verbs.retain(|v| v != "hide" && v != "show");
        }
        let condition = exprs(r).first().map(|x| self.expr(x));
        Rule {
            effect,
            verbs,
            history,
            targets,
            condition,
            span,
        }
    }

    fn print_opts(&mut self, n: &SyntaxNode) -> Vec<PrintOpt> {
        ast::children(n, PRINT_OPT)
            .map(|o| PrintOpt {
                name: ctx_words(&o).into_iter().next().unwrap_or_default(),
                value: exprs(&o)
                    .first()
                    .map(|e| self.expr(e))
                    .or_else(|| self.word(&o)),
                span: self.span(&o),
            })
            .collect()
    }

    /// A word of an element (`page A4`, `align right`): a NAME, read as a name expression.
    fn word(&mut self, n: &SyntaxNode) -> Option<Expr> {
        let w = ast::child(n, NAME)?;
        Some(Expr {
            kind: ExprKind::Name(w.text().to_string().trim().to_string()),
            span: self.span(&w),
        })
    }

    /// The elements among a node's children (all of them, or those before/after `else`).
    fn print_items(&mut self, n: &SyntaxNode) -> Vec<PrintItem> {
        n.children()
            .filter(|c| matches!(c.kind(), PRINT_ITEM | PRINT_LET | PRINT_IF | PRINT_FOR))
            .map(|c| self.print_item(&c))
            .collect()
    }

    fn print_item(&mut self, n: &SyntaxNode) -> PrintItem {
        let span = self.span(n);
        let kind = match n.kind() {
            PRINT_LET => PrintKind::Let {
                name: ast::name_text(n).unwrap_or_default(),
                value: exprs(n)
                    .first()
                    .map(|e| self.expr(e))
                    .unwrap_or_else(|| self.error_expr(n)),
            },
            PRINT_FOR => PrintKind::For {
                name: ast::name_text(n).unwrap_or_default(),
                iter: exprs(n)
                    .first()
                    .map(|e| self.expr(e))
                    .unwrap_or_else(|| self.error_expr(n)),
                body: self.print_items(n),
            },
            PRINT_IF => {
                let cond = exprs(n)
                    .first()
                    .map(|e| self.expr(e))
                    .unwrap_or_else(|| self.error_expr(n));
                let mut then = Vec::new();
                let mut otherwise = Vec::new();
                let mut after_else = false;
                for c in n.children_with_tokens() {
                    match c {
                        rowan::NodeOrToken::Token(t) if t.kind() == ELSE_KW => after_else = true,
                        rowan::NodeOrToken::Node(c)
                            if matches!(
                                c.kind(),
                                PRINT_ITEM | PRINT_LET | PRINT_IF | PRINT_FOR
                            ) =>
                        {
                            let it = self.print_item(&c);
                            if after_else {
                                otherwise.push(it)
                            } else {
                                then.push(it)
                            }
                        }
                        _ => {}
                    }
                }
                PrintKind::If {
                    bind: ast::name_text(n),
                    cond,
                    then,
                    otherwise,
                }
            }
            _ => {
                let element = ctx_words(n).into_iter().next().unwrap_or_default();
                // the value is the expression before the `:`, the body the one after it
                let mut value = None;
                let mut body = None;
                let mut after_colon = false;
                for c in n.children_with_tokens() {
                    match c {
                        rowan::NodeOrToken::Token(t) if t.kind() == COLON => after_colon = true,
                        rowan::NodeOrToken::Node(c) if is_expr(&c) => {
                            let e = self.expr(&c);
                            if after_colon {
                                body = Some(e)
                            } else {
                                value = Some(e)
                            }
                        }
                        _ => {}
                    }
                }
                let (value, binding) = if element == "page" && value.is_none() {
                    (self.word(n), None)
                } else {
                    (value, ast::name_text(n))
                };
                PrintKind::Element {
                    element,
                    value,
                    opts: self.print_opts(n),
                    binding,
                    body,
                    children: self.print_items(n),
                }
            }
        };
        PrintItem { kind, span }
    }

    fn fn_def(&mut self, n: &SyntaxNode) -> FnDef {
        let name = ast::name_text(n).unwrap_or_default();
        let params = ast::child(n, PARAM_LIST)
            .map(|p| self.params(&p))
            .unwrap_or_default();
        let ret = ast::child(n, RET_TYPE)
            .and_then(|r| type_node(&r))
            .map(|t| self.ty(&t));
        let builtin = ctx_words(n).iter().any(|w| w == "builtin");
        let body = ast::child(n, BLOCK).map(|b| self.stmts(&b));
        FnDef {
            name,
            params,
            ret,
            body,
            builtin,
            span: self.span(n),
        }
    }

    fn params(&mut self, list: &SyntaxNode) -> Vec<Param> {
        ast::children(list, PARAM)
            .map(|p| Param {
                name: ast::name_text(&p).unwrap_or_default(),
                ty: type_node(&p).map(|t| self.ty(&t)),
                default: exprs(&p).first().map(|e| self.expr(e)),
            })
            .collect()
    }

    fn variant(&mut self, v: &SyntaxNode) -> Variant {
        Variant {
            name: ast::name_text(v).unwrap_or_default(),
            fields: ast::child(v, FIELD_LIST)
                .map(|l| self.tuple_fields(&l))
                .unwrap_or_default(),
        }
    }

    fn tuple_fields(&mut self, list: &SyntaxNode) -> Vec<(String, Ty)> {
        ast::children(list, TUPLE_FIELD)
            .map(|f| {
                (
                    ast::name_text(&f).unwrap_or_default(),
                    type_node(&f).map(|t| self.ty(&t)).unwrap_or(Ty::Error),
                )
            })
            .collect()
    }

    // ---- types ------------------------------------------------------------------------

    /// What a `File` type takes (`File<pdf, "application/zip">`, inside `?`, `[…]`, `{…}`),
    /// if the type is one.
    fn file_spec(&mut self, n: &SyntaxNode) -> Option<files::FileSpec> {
        match n.kind() {
            TYPE_OPTIONAL | TYPE_LIST | TYPE_SET | TYPE_PAREN => {
                type_node(n).and_then(|t| self.file_spec(&t))
            }
            TYPE_PATH => {
                let path = ast::child(n, PATH).map(|p| ast::path_text(&p))?;
                if path != "File" {
                    return None;
                }
                let mut spec = files::FileSpec::default();
                if let Some(g) = ast::child(n, GENERIC_ARGS) {
                    for el in g.children_with_tokens() {
                        match el {
                            rowan::NodeOrToken::Token(t) if t.kind() == STRING => spec
                                .accept
                                .push((t.text().to_string(), Span::new(self.file, t.text_range()))),
                            rowan::NodeOrToken::Node(c) if TYPE_KINDS.contains(&c.kind()) => {
                                let name = ast::child(&c, PATH)
                                    .map(|p| ast::path_text(&p))
                                    .unwrap_or_else(|| c.text().to_string());
                                spec.accept.push((name, self.span(&c)));
                            }
                            _ => {}
                        }
                    }
                }
                Some(spec)
            }
            _ => None,
        }
    }

    fn ty(&mut self, n: &SyntaxNode) -> Ty {
        let inner = |s: &mut Self| type_node(n).map(|t| s.ty(&t)).unwrap_or(Ty::Error);
        match n.kind() {
            TYPE_PATH => {
                let segments = ast::child(n, PATH)
                    .map(|p| path_segments(&p))
                    .unwrap_or_default();
                let args = ast::child(n, GENERIC_ARGS)
                    .map(|g| {
                        g.children()
                            .filter(|c| TYPE_KINDS.contains(&c.kind()))
                            .map(|c| self.ty(&c))
                            .collect()
                    })
                    .unwrap_or_default();
                Ty::Path {
                    segments,
                    args,
                    span: self.span(n),
                }
            }
            TYPE_OPTIONAL => Ty::Optional(Box::new(inner(self))),
            TYPE_LIST => Ty::List(Box::new(inner(self))),
            TYPE_SET => Ty::Set(Box::new(inner(self))),
            TYPE_PAREN => inner(self),
            TYPE_TUPLE => Ty::Tuple(self.tuple_fields(n)),
            TYPE_ENUM => Ty::Enum(
                ast::children(n, VARIANT)
                    .map(|v| self.variant(&v))
                    .collect(),
            ),
            TYPE_UNION => Ty::Union(
                n.children()
                    .filter(|c| TYPE_KINDS.contains(&c.kind()))
                    .map(|c| self.ty(&c))
                    .collect(),
            ),
            _ => Ty::Error,
        }
    }

    // ---- statements -------------------------------------------------------------------

    /// The statements of the BLOCK child of `n`.
    fn block_of(&mut self, n: &SyntaxNode) -> Vec<Stmt> {
        ast::child(n, BLOCK)
            .map(|b| self.stmts(&b))
            .unwrap_or_default()
    }

    fn stmts(&mut self, block: &SyntaxNode) -> Vec<Stmt> {
        block.children().filter_map(|s| self.stmt(&s)).collect()
    }

    fn stmt(&mut self, s: &SyntaxNode) -> Option<Stmt> {
        let span = self.span(s);
        let es = exprs(s);
        let first = |l: &mut Self| es.first().map(|e| l.expr(e)).unwrap_or(l.error_expr(s));
        let kind = match s.kind() {
            LET_STMT => {
                let name = ast::name_text(s).unwrap_or_default();
                let ty = type_node(s).map(|t| self.ty(&t));
                let value = ast::child(s, VALUE)
                    .map(|v| self.value(&v))
                    .unwrap_or(self.error_expr(s));
                StmtKind::Let { name, ty, value }
            }
            IF_STMT => {
                let binding = ast::token(s, LET_KW).and_then(|_| ast::name_text(s));
                let cond = first(self);
                let then = ast::child(s, BLOCK)
                    .map(|b| self.stmts(&b))
                    .unwrap_or_default();
                let otherwise = ast::child(s, ELSE_BRANCH).map(|e| {
                    if let Some(nested) = ast::child(&e, IF_STMT) {
                        self.stmt(&nested).into_iter().collect()
                    } else {
                        ast::child(&e, BLOCK)
                            .map(|b| self.stmts(&b))
                            .unwrap_or_default()
                    }
                });
                StmtKind::If {
                    binding,
                    cond,
                    then,
                    otherwise,
                }
            }
            FOR_STMT => {
                let names = ast::children(s, NAME)
                    .filter_map(|n| ast::token(&n, IDENT))
                    .map(|t| t.text().to_string())
                    .collect();
                let iter = first(self);
                let body = self.block_of(s);
                StmtKind::For { names, iter, body }
            }
            RETURN_STMT => StmtKind::Return(es.first().map(|e| self.expr(e))),
            KEYWORD_STMT => {
                let kw = ast::ctx_keyword(s).unwrap_or_default();
                if let Some(b) = ast::child(s, BLOCK) {
                    let body = self.stmts(&b);
                    StmtKind::Block { keyword: kw, body }
                } else {
                    match kw.as_str() {
                        "require" => StmtKind::Require {
                            cond: first(self),
                            message: es.get(1).map(|e| self.expr(e)),
                        },
                        "erase" => StmtKind::Erase {
                            target: first(self),
                            reason: es.get(1).map(|e| self.expr(e)),
                        },
                        "fail" => StmtKind::Fail(first(self)),
                        "emit" => StmtKind::Emit(first(self)),
                        "expect" => StmtKind::Expect(first(self)),
                        "clock" => StmtKind::Clock(first(self)),
                        "use" => StmtKind::Use(
                            s.descendants()
                                .find(|n| n.kind() == NAME_REF)
                                .and_then(|n| ast::token(&n, IDENT))
                                .map(|t| t.text().to_string())
                                .unwrap_or_default(),
                        ),
                        _ => StmtKind::Expr(first(self)),
                    }
                }
            }
            TEST_STMT => {
                let words = ctx_words(s);
                let target = ast::child(s, NAME)
                    .map(|n| n.text().to_string().trim().to_string())
                    .unwrap_or_default();
                match words.first().map(String::as_str) {
                    Some("receive") => StmtKind::Receive {
                        inbound: target,
                        body: first(self),
                        file: words.iter().any(|w| w == "file"),
                    },
                    Some("deliver") => {
                        let value = es.first().map(|e| self.expr(e));
                        let failed = words.iter().any(|w| w == "failed");
                        StmtKind::Deliver {
                            event: target,
                            reply: if failed { None } else { value.clone() },
                            failed: if failed { value } else { None },
                        }
                    }
                    _ => StmtKind::ExpectSent {
                        event: first(self),
                        negated: ast::token(s, NOT_KW).is_some(),
                    },
                }
            }
            EXPECT_BLOCK => {
                let kw = ast::ctx_keyword(s).unwrap_or_default();
                let message = tokens(s)
                    .find(|t| t.kind() == STRING)
                    .map(|t| unquote(t.text()))
                    .unwrap_or_default();
                let block = ast::child(s, BLOCK);
                if kw == "expect_compile_error" {
                    let source = block
                        .map(|b| dedent(&b.text().to_string()))
                        .unwrap_or_default();
                    StmtKind::ExpectCompileError { message, source }
                } else {
                    let body = block.map(|b| self.stmts(&b)).unwrap_or_default();
                    StmtKind::ExpectError { message, body }
                }
            }
            AS_STMT => {
                let user = ast::child(s, NAME_REF)
                    .and_then(|n| ast::token(&n, IDENT))
                    .map(|t| t.text().to_string())
                    .unwrap_or_default();
                let body = self.block_of(s);
                StmtKind::As { user, body }
            }
            ASSIGN_STMT => {
                let op = tokens(s)
                    .find_map(|t| match t.kind() {
                        EQ => Some(AssignOp::Set),
                        PLUS_EQ => Some(AssignOp::Add),
                        MINUS_EQ => Some(AssignOp::Sub),
                        _ => None,
                    })
                    .unwrap_or(AssignOp::Set);
                let target = first(self);
                let value = es
                    .get(1)
                    .map(|e| self.expr(e))
                    .unwrap_or(self.error_expr(s));
                StmtKind::Assign { target, op, value }
            }
            EXPR_STMT => StmtKind::Expr(first(self)),
            META => {
                let key = ast::name_text(s).unwrap_or_default();
                let value = if let Some(p) = ast::child(s, PATH) {
                    Expr::new(ExprKind::Name(ast::path_text(&p)), self.span(&p))
                } else {
                    first(self)
                };
                StmtKind::Meta { key, value }
            }
            FN => StmtKind::Fn(self.fn_def(s)),
            MIGRATION_STEP => {
                let keyword = ast::ctx_keyword(s).unwrap_or_default();
                let paths = ast::children(s, PATH).map(|p| path_segments(&p)).collect();
                let body = self.block_of(s);
                let arms = s
                    .children()
                    .filter(|n| n.kind() == MATCH_ARM)
                    .map(|a| self.arm(&a))
                    .collect();
                StmtKind::Migration {
                    keyword,
                    paths,
                    body,
                    arms,
                }
            }
            _ => return None,
        };
        Some(Stmt { kind, span })
    }

    // ---- expressions ------------------------------------------------------------------

    fn error_expr(&self, n: &SyntaxNode) -> Expr {
        Expr::new(ExprKind::Error, self.span(n))
    }

    /// VALUE / DEFAULT_VALUE: an expression or a block.
    fn value(&mut self, v: &SyntaxNode) -> Expr {
        match exprs(v).first() {
            Some(e) => self.expr(e),
            None => self.error_expr(v),
        }
    }

    fn arm(&mut self, a: &SyntaxNode) -> Arm {
        let patterns = ast::children(a, PATTERN)
            .map(|p| self.pattern(&p))
            .collect();
        let body = if let Some(b) = ast::child(a, BLOCK) {
            let stmts = self.stmts(&b);
            Expr::new(ExprKind::Block(stmts), self.span(&b))
        } else {
            exprs(a)
                .first()
                .map(|e| self.expr(e))
                .unwrap_or(self.error_expr(a))
        };
        Arm { patterns, body }
    }

    fn pattern(&mut self, p: &SyntaxNode) -> Pattern {
        let toks: Vec<SyntaxToken> = tokens(p).collect();
        if let Some(nr) = ast::child(p, NAME_REF) {
            let name = ast::token(&nr, IDENT)
                .map(|t| t.text().to_string())
                .unwrap_or_default();
            if name == "_" {
                return Pattern::Wildcard;
            }
            if toks.iter().any(|t| t.kind() == L_PAREN) {
                let args = ast::children(p, PATTERN)
                    .map(|c| self.pattern(&c))
                    .collect();
                return Pattern::Ctor { name, args };
            }
            return Pattern::Name(name);
        }
        match toks.first() {
            Some(t) => Pattern::Lit(self.lit(t)),
            None => Pattern::Wildcard,
        }
    }

    fn lit(&self, t: &SyntaxToken) -> Lit {
        let text = t.text();
        match t.kind() {
            NUMBER => Lit::Number(text.replace('_', "")),
            PERCENT => Lit::Percent(text.trim_end_matches('%').replace('_', "")),
            STRING => Lit::Str(unquote(text)),
            DATE => Lit::Date(text.to_string()),
            DATETIME => Lit::DateTime(text.replace(' ', "T")),
            YEAR_MONTH => Lit::YearMonth(text.to_string()),
            TRUE_KW => Lit::Bool(true),
            FALSE_KW => Lit::Bool(false),
            _ => Lit::None,
        }
    }

    /// `f"…{expr}…{expr:spec}…"`: `{{` and `}}` are literal braces, escapes as in strings.
    fn fstring(&mut self, t: &SyntaxToken, span: Span) -> ExprKind {
        let raw = t.text();
        let body = if raw.starts_with("f\"\"\"") {
            &raw[4..raw.len().saturating_sub(3)]
        } else {
            &raw[2..raw.len().saturating_sub(1)]
        };
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut chars = body.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => {
                    let next = chars.next().map(|(_, n)| n);
                    text.push_str(&crate::unescape(&format!(
                        "\\{}",
                        next.map(String::from).unwrap_or_default()
                    )));
                }
                '{' if chars.peek().map(|(_, n)| *n) == Some('{') => {
                    chars.next();
                    text.push('{');
                }
                '}' if chars.peek().map(|(_, n)| *n) == Some('}') => {
                    chars.next();
                    text.push('}');
                }
                '{' => {
                    let mut depth = 1;
                    let start = i + 1;
                    let mut end = body.len();
                    for (j, d) in chars.by_ref() {
                        if d == '{' {
                            depth += 1;
                        } else if d == '}' {
                            depth -= 1;
                            if depth == 0 {
                                end = j;
                                break;
                            }
                        }
                    }
                    if !text.is_empty() {
                        parts.push(FPart::Text(std::mem::take(&mut text)));
                    }
                    let inner = &body[start..end];
                    let (code, spec) = match top_level_colon(inner) {
                        Some(at) => {
                            let spec = &inner[at + 1..];
                            match FormatSpec::parse(spec) {
                                Some(s) => (&inner[..at], Some(s)),
                                None => {
                                    self.model.errors.push((span, format!("invalid format `{spec}` in f-string: write `[<|>|^][0][width][.precision]`, e.g. `{{n:05}}` or `{{x:.2}}`")));
                                    (&inner[..at], None)
                                }
                            }
                        }
                        None => (inner, None),
                    };
                    let expr = match quadra_syntax::parse_expr(code) {
                        Some(n) => {
                            let mut e = self.expr(&n);
                            relocate(&mut e, span);
                            e
                        }
                        None => {
                            self.model.errors.push((span, format!("invalid expression in f-string: `{{{inner}}}` (write `{{{{` for a literal brace)")));
                            Expr::new(ExprKind::Error, span)
                        }
                    };
                    parts.push(FPart::Expr(expr, spec));
                }
                _ => text.push(c),
            }
        }
        if !text.is_empty() {
            parts.push(FPart::Text(text));
        }
        ExprKind::FString(parts)
    }

    pub(crate) fn expr(&mut self, n: &SyntaxNode) -> Expr {
        let span = self.span(n);
        let es = exprs(n);
        let sub = |l: &mut Self, i: usize| {
            es.get(i)
                .map(|e| l.expr(e))
                .map(Box::new)
                .unwrap_or(Box::new(l.error_expr(n)))
        };
        let kind = match n.kind() {
            LITERAL => match tokens(n).next() {
                Some(t) if t.kind() == FSTRING => self.fstring(&t, span),
                Some(t) => ExprKind::Lit(self.lit(&t)),
                None => ExprKind::Error,
            },
            QUANTITY => {
                let value = tokens(n)
                    .next()
                    .map(|t| t.text().replace('_', ""))
                    .unwrap_or_default();
                let units: Vec<String> = ast::child(n, UNIT)
                    .map(|u| {
                        tokens(&u)
                            .filter(|t| t.kind() == IDENT)
                            .map(|t| t.text().to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                ExprKind::Quantity {
                    value,
                    unit: units.first().cloned().unwrap_or_default(),
                    per: units.get(1).cloned(),
                }
            }
            NAME_REF => {
                let text = ast::token(n, IDENT)
                    .map(|t| t.text().to_string())
                    .unwrap_or_default();
                if text == "self" {
                    ExprKind::SelfRef
                } else {
                    ExprKind::Name(text)
                }
            }
            FIELD_EXPR => {
                let base = sub(self, 0);
                let name = n
                    .children()
                    .filter(|c| c.kind() == NAME_REF)
                    .last()
                    .and_then(|c| ast::token(&c, IDENT))
                    .map(|t| t.text().to_string())
                    .unwrap_or_default();
                let optional = ast::token(n, QUESTION_DOT).is_some();
                ExprKind::Field {
                    base,
                    name,
                    optional,
                }
            }
            CALL_EXPR => {
                let callee = sub(self, 0);
                let args = ast::child(n, ARG_LIST)
                    .map(|l| ast::children(&l, ARG).map(|a| self.arg(&a)).collect())
                    .unwrap_or_default();
                ExprKind::Call { callee, args }
            }
            INDEX_EXPR => ExprKind::Index {
                base: sub(self, 0),
                index: sub(self, 1),
            },
            PREFIX_EXPR => {
                let op = if ast::token(n, NOT_KW).is_some() {
                    UnOp::Not
                } else {
                    UnOp::Neg
                };
                ExprKind::Unary {
                    op,
                    expr: sub(self, 0),
                }
            }
            BIN_EXPR => {
                let toks: Vec<SyntaxToken> = tokens(n).collect();
                let has = |k: SyntaxKind| toks.iter().any(|t| t.kind() == k);
                let op = if has(IS_KW) {
                    if has(NOT_KW) {
                        BinOp::IsNot
                    } else {
                        BinOp::Is
                    }
                } else if has(IN_KW) {
                    if has(NOT_KW) {
                        BinOp::NotIn
                    } else {
                        BinOp::In
                    }
                } else {
                    match toks.first().map(|t| (t.kind(), t.text().to_string())) {
                        Some((PLUS, _)) => BinOp::Add,
                        Some((MINUS, _)) => BinOp::Sub,
                        Some((STAR, _)) => BinOp::Mul,
                        Some((SLASH, _)) => BinOp::Div,
                        Some((EQ2, _)) => BinOp::Eq,
                        Some((NEQ, _)) => BinOp::Ne,
                        Some((LT, _)) => BinOp::Lt,
                        Some((LE, _)) => BinOp::Le,
                        Some((GT, _)) => BinOp::Gt,
                        Some((GE, _)) => BinOp::Ge,
                        Some((AND_KW, _)) => BinOp::And,
                        Some((OR_KW, _)) => BinOp::Or,
                        Some((QUESTION2, _)) => BinOp::Coalesce,
                        Some((DOT2, _)) => BinOp::Range,
                        Some((_, t)) if t == "implies" => BinOp::Implies,
                        _ => BinOp::Eq,
                    }
                };
                ExprKind::Binary {
                    op,
                    lhs: sub(self, 0),
                    rhs: sub(self, 1),
                }
            }
            IF_EXPR => ExprKind::If {
                then: sub(self, 0),
                cond: sub(self, 1),
                otherwise: sub(self, 2),
            },
            LAMBDA_EXPR => {
                let params = ast::child(n, PARAM_LIST)
                    .map(|l| {
                        ast::children(&l, PARAM)
                            .filter_map(|p| ast::name_text(&p))
                            .collect()
                    })
                    .unwrap_or_default();
                ExprKind::Lambda {
                    params,
                    body: std::sync::Arc::from(sub(self, 0)),
                }
            }
            PAREN_EXPR => {
                return es
                    .first()
                    .map(|e| self.expr(e))
                    .unwrap_or(self.error_expr(n))
            }
            TUPLE_EXPR => {
                let mut items = Vec::new();
                for c in n.children() {
                    if c.kind() == NAMED_ITEM {
                        let name = ast::name_text(&c);
                        let v = exprs(&c)
                            .first()
                            .map(|e| self.expr(e))
                            .unwrap_or(self.error_expr(&c));
                        items.push((name, v));
                    } else if is_expr(&c) {
                        items.push((None, self.expr(&c)));
                    }
                }
                ExprKind::Tuple(items)
            }
            LIST_EXPR => ExprKind::List(
                n.children()
                    .filter(|c| c.kind() == ARG || is_expr(c))
                    .map(|c| {
                        if c.kind() == ARG {
                            self.arg(&c)
                        } else {
                            Arg {
                                name: None,
                                value: self.expr(&c),
                                spread: false,
                            }
                        }
                    })
                    .collect(),
            ),
            SET_EXPR => ExprKind::Set(es.iter().map(|e| self.expr(e)).collect()),
            WHEN_EXPR => ExprKind::When(
                ast::children(n, WHEN_ARM)
                    .map(|a| {
                        let xs = exprs(&a);
                        if ast::token(&a, ELSE_KW).is_some() {
                            (
                                None,
                                xs.first()
                                    .map(|e| self.expr(e))
                                    .unwrap_or(self.error_expr(&a)),
                            )
                        } else {
                            let c = xs
                                .first()
                                .map(|e| self.expr(e))
                                .unwrap_or(self.error_expr(&a));
                            let v = xs
                                .get(1)
                                .map(|e| self.expr(e))
                                .unwrap_or(self.error_expr(&a));
                            (Some(c), v)
                        }
                    })
                    .collect(),
            ),
            MATCH_EXPR => {
                let subject = sub(self, 0);
                let arms = ast::children(n, MATCH_ARM).map(|a| self.arm(&a)).collect();
                ExprKind::Match { subject, arms }
            }
            BLOCK_EXPR => ExprKind::Block(
                ast::child(n, BLOCK)
                    .map(|b| self.stmts(&b))
                    .unwrap_or_default(),
            ),
            AS_EXPR => {
                let user = ast::child(n, NAME_REF)
                    .and_then(|c| ast::token(&c, IDENT))
                    .map(|t| t.text().to_string())
                    .unwrap_or_default();
                let body = es
                    .last()
                    .filter(|_| es.len() > 1)
                    .map(|e| self.expr(e))
                    .unwrap_or(self.error_expr(n));
                ExprKind::As {
                    user,
                    body: Box::new(body),
                }
            }
            FAIL_EXPR => ExprKind::Fail(sub(self, 0)),
            _ => ExprKind::Error,
        };
        Expr::new(kind, span)
    }

    fn arg(&mut self, a: &SyntaxNode) -> Arg {
        let name = ast::name_text(a);
        let spread = ast::token(a, STAR).is_some();
        let value = exprs(a)
            .first()
            .map(|e| self.expr(e))
            .unwrap_or(self.error_expr(a));
        Arg {
            name,
            value,
            spread,
        }
    }
}

/// Expressions parsed from inside an f-string get the span of the whole string.
/// The position of the last `:` outside brackets in an interpolation (`{n:05}`).
fn top_level_colon(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut at = None;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ':' if depth == 0 => at = Some(i),
            _ => {}
        }
    }
    at
}

fn relocate(e: &mut Expr, span: Span) {
    e.span = span;
    match &mut e.kind {
        ExprKind::Field { base, .. } => relocate(base, span),
        ExprKind::Call { callee, args } => {
            relocate(callee, span);
            for a in args {
                relocate(&mut a.value, span);
            }
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            relocate(lhs, span);
            relocate(rhs, span);
        }
        ExprKind::Unary { expr, .. } => relocate(expr, span),
        ExprKind::If {
            cond,
            then,
            otherwise,
        } => {
            relocate(cond, span);
            relocate(then, span);
            relocate(otherwise, span);
        }
        _ => {}
    }
}
