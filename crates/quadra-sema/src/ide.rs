//! Editor features built on the analysis: go to definition, hover, document outline.

use crate::{Analysis, DeclId, DeclKind, FileId, MemberKind, Project, Target};
use quadra_syntax::{ast, SyntaxKind::*, SyntaxNode, SyntaxToken, TextRange, TextSize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NavTarget {
    pub file: FileId,
    pub range: TextRange,
}

#[derive(Clone, Debug)]
pub enum SymbolKind {
    Decl(DeclKind),
    Member(MemberKind),
    Extend,
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub detail: String,
    pub kind: SymbolKind,
    pub range: TextRange,
    pub selection: TextRange,
    pub children: Vec<Symbol>,
}

/// The identifier token at `offset` (or just before it, when the cursor is at its end).
pub fn ident_at(project: &Project, file: FileId, offset: TextSize) -> Option<SyntaxToken> {
    let root = project.file(file).parse.syntax();
    let pick = |t: SyntaxToken| (t.kind() == IDENT || t.kind() == CONTEXT_KW).then_some(t);
    match root.token_at_offset(offset) {
        rowan::TokenAtOffset::None => None,
        rowan::TokenAtOffset::Single(t) => pick(t),
        rowan::TokenAtOffset::Between(a, b) => pick(b).or_else(|| pick(a)),
    }
}

/// What the identifier at `offset` refers to.
pub fn target_at(
    project: &Project,
    a: &Analysis,
    file: FileId,
    offset: TextSize,
) -> Option<(TextRange, Target)> {
    let tok = ident_at(project, file, offset)?;
    let range = tok.text_range();
    if let Some(r) = a
        .references
        .iter()
        .find(|r| r.file == file && r.range == range)
    {
        return Some((range, r.target.clone()));
    }
    // a declaration's own name
    if let Some(d) = a
        .decls
        .iter()
        .position(|d| d.file == file && d.name_range == range)
    {
        return Some((range, Target::Decl(DeclId(d as u32))));
    }
    for (owner, members) in &a.members {
        if let Some(i) = members
            .iter()
            .position(|m| m.file == file && m.name_range == range)
        {
            return Some((range, Target::Member(*owner, i)));
        }
    }
    // best effort for lower-case names: a member of the enclosing entity, else a
    // function, constant or fixture in scope
    let name = tok.text();
    if let Some(owner) = enclosing_owner(a, file, &tok) {
        if let Some((d, i)) = a.lookup_member(owner, name) {
            return Some((range, Target::Member(d, i)));
        }
    }
    a.resolve(file, name).map(|d| (range, Target::Decl(d)))
}

fn enclosing_owner(a: &Analysis, file: FileId, tok: &SyntaxToken) -> Option<DeclId> {
    let item = tok
        .parent_ancestors()
        .find(|n| matches!(n.kind(), ENTITY | LEDGER | METRIC | CONTROL))?;
    if ast::token(&item, EXTEND_KW).is_some() || ast::ctx_keyword(&item).as_deref() == Some("patch")
    {
        let path = ast::child(&item, PATH)?;
        let seg = ast::path_segments(&path).into_iter().next()?;
        return a.resolve(file, seg.text());
    }
    let name = ast::name_text(&item)?;
    a.module_decls.get(&file)?.get(&name).copied()
}

pub fn definition(
    project: &Project,
    a: &Analysis,
    file: FileId,
    offset: TextSize,
) -> Option<NavTarget> {
    let (_, target) = target_at(project, a, file, offset)?;
    match target {
        Target::Decl(d) => {
            let d = a.decl(d);
            Some(NavTarget {
                file: d.file,
                range: d.name_range,
            })
        }
        Target::Member(o, i) => {
            let m = a.member(o, i);
            Some(NavTarget {
                file: m.file,
                range: m.name_range,
            })
        }
        Target::Variant(name) => {
            // a variant name can be defined by several entities: prefer the current file
            let defs = a.variants.get(&name)?;
            let (f, range) = *defs
                .iter()
                .find(|(f, _)| *f == file)
                .or_else(|| defs.first())?;
            Some(NavTarget { file: f, range })
        }
    }
}

fn module_of(project: &Project, file: FileId) -> &str {
    &project.file(file).module
}

fn code(s: &str) -> String {
    format!("```quadra\n{s}\n```")
}

pub fn hover(
    project: &Project,
    a: &Analysis,
    file: FileId,
    offset: TextSize,
) -> Option<(TextRange, String)> {
    let (range, target) = target_at(project, a, file, offset)?;
    Some((range, hover_target(project, a, target)))
}

/// What a hover shows of a declaration, a member, a variant.
pub fn hover_target(project: &Project, a: &Analysis, target: Target) -> String {
    match target {
        Target::Decl(id) => hover_decl(project, a, id),
        Target::Member(owner, i) => {
            let m = a.member(owner, i);
            let o = a.decl(owner);
            let mut s = code(&m.header);
            if let Some(doc) = &m.doc {
                s.push_str(&format!("\n\n{doc}"));
            }
            s.push_str(&format!("\n\n{} of `{}`", m.kind.describe(), o.name));
            if m.via_extend {
                s.push_str(&format!(
                    ", added by `extend` in `{}`",
                    module_of(project, m.file)
                ));
            }
            if !m.variants.is_empty() {
                s.push_str(&format!("\n\nValues: {}", m.variants.join(" · ")));
            }
            s
        }
        Target::Variant(name) => {
            let mut s = format!("variant `{name}`");
            if let Some(defs) = a.variants.get(&name) {
                let places: Vec<String> = defs
                    .iter()
                    .map(|(f, r)| {
                        let text = &project.file(*f).text;
                        let line = text[..usize::from(r.start())].matches('\n').count() + 1;
                        format!("`{}`:{line}", module_of(project, *f))
                    })
                    .collect();
                s.push_str(&format!("\n\ndefined in {}", places.join(", ")));
            }
            s
        }
    }
}

/// Where a declaration, a member or a variant is declared (`file`: prefer a variant of it).
pub fn target_definition(a: &Analysis, target: &Target, file: FileId) -> Option<NavTarget> {
    match target {
        Target::Decl(d) => {
            let d = a.decl(*d);
            Some(NavTarget {
                file: d.file,
                range: d.name_range,
            })
        }
        Target::Member(o, i) => {
            let m = a.member(*o, *i);
            Some(NavTarget {
                file: m.file,
                range: m.name_range,
            })
        }
        Target::Variant(name) => {
            let defs = a.variants.get(name)?;
            let (f, range) = *defs
                .iter()
                .find(|(f, _)| *f == file)
                .or_else(|| defs.first())?;
            Some(NavTarget { file: f, range })
        }
    }
}

/// Every place that names `target`: its declaration and its references (in every file the
/// analysis has references of, `.qdi` files included), each once.
pub fn references(a: &Analysis, target: &Target) -> Vec<NavTarget> {
    let mut out: Vec<NavTarget> = Vec::new();
    match target {
        Target::Decl(d) => {
            let d = a.decl(*d);
            out.push(NavTarget {
                file: d.file,
                range: d.name_range,
            });
        }
        Target::Member(o, i) => {
            let m = a.member(*o, *i);
            out.push(NavTarget {
                file: m.file,
                range: m.name_range,
            });
        }
        Target::Variant(name) => {
            for (f, r) in a.variants.get(name).into_iter().flatten() {
                out.push(NavTarget {
                    file: *f,
                    range: *r,
                });
            }
        }
    }
    for r in &a.references {
        if &r.target == target {
            out.push(NavTarget {
                file: r.file,
                range: r.range,
            });
        }
    }
    out.sort_by_key(|n| (n.file, n.range.start()));
    out.dedup();
    out
}

/// The target of a reference recorded at `offset` of `file` (a file without a syntax tree of
/// Quadra code: an interface file; or a member the type checker resolved).
pub fn reference_at(a: &Analysis, file: FileId, offset: TextSize) -> Option<(TextRange, Target)> {
    a.references
        .iter()
        .find(|r| r.file == file && r.range.start() <= offset && offset <= r.range.end())
        .map(|r| (r.range, r.target.clone()))
}

/// Hover for a declaration. For entities it shows the unified view: own members, members
/// added by `extend` in other modules, and those inherited from traits.
fn hover_decl(project: &Project, a: &Analysis, id: DeclId) -> String {
    let d = a.decl(id);
    let mut s = code(&d.header);
    if let Some(doc) = &d.doc {
        s.push_str(&format!("\n\n{doc}"));
    }
    s.push_str(&format!(
        "\n\n{} in `{}`",
        d.kind.describe(),
        module_of(project, d.file)
    ));
    if d.kind.has_members() {
        let members = a.all_members(id);
        if !members.is_empty() {
            s.push_str("\n\n---\n");
            for (owner, i) in members {
                let m = a.member(owner, i);
                let origin = if owner != id {
                    format!("  *(from trait `{}`)*", a.decl(owner).name)
                } else if m.via_extend {
                    format!("  *(extend in `{}`)*", module_of(project, m.file))
                } else {
                    String::new()
                };
                s.push_str(&format!("\n- `{}`{origin}", m.header));
            }
        }
    }
    s
}

pub fn document_symbols(project: &Project, a: &Analysis, file: FileId) -> Vec<Symbol> {
    let root: SyntaxNode = project.file(file).parse.syntax();
    let mut out = Vec::new();
    for item in root.children() {
        let members_of = |owner: DeclId| -> Vec<Symbol> {
            a.members
                .get(&owner)
                .map(|ms| {
                    ms.iter()
                        .filter(|m| {
                            m.file == file && item.text_range().contains_range(m.name_range)
                        })
                        .map(|m| Symbol {
                            name: m.name.clone(),
                            detail: m.header.clone(),
                            kind: SymbolKind::Member(m.kind),
                            range: m.name_range,
                            selection: m.name_range,
                            children: Vec::new(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        if let Some((idx, d)) = a
            .decls
            .iter()
            .enumerate()
            .find(|(_, d)| d.file == file && d.range == item.text_range())
        {
            out.push(Symbol {
                name: d.name.clone(),
                detail: d.kind.describe().to_string(),
                kind: SymbolKind::Decl(d.kind),
                range: d.range,
                selection: d.name_range,
                children: members_of(DeclId(idx as u32)),
            });
        } else if item.kind() == ENTITY && ast::token(&item, EXTEND_KW).is_some() {
            let Some(path) = ast::child(&item, PATH) else {
                continue;
            };
            let name = ast::path_text(&path);
            let children = a.resolve(file, &name).map(members_of).unwrap_or_default();
            out.push(Symbol {
                name: format!("extend {name}"),
                detail: String::new(),
                kind: SymbolKind::Extend,
                range: item.text_range(),
                selection: path.text_range(),
                children,
            });
        }
    }
    out
}

/// The methods of collections (and of entities, which are collections of their records), for
/// hover and completion after a `.`: (name, signature, what it gives).
pub const METHODS: &[(&str, &str, &str)] = &[
    ("where", "where(x => cond)", "the elements that satisfy `cond`"),
    ("map", "map(x => v)", "a list of the values"),
    ("flat_map", "flat_map(x => coll)", "a list: the collections concatenated"),
    ("sum", "sum() · sum(x => v)", "the sum of the values (none skipped; one currency, one unit)"),
    ("avg", "avg() · avg(x => v)", "the mean of the values that are not none; none when there is none"),
    ("count", "count() · count(x => cond)", "how many (that satisfy `cond`)"),
    ("any", "any() · any(x => cond)", "at least one element (that satisfies `cond`)"),
    ("all", "all(x => cond)", "every element satisfies `cond` (true when empty); `Entity.all()`: every record"),
    ("none", "none() · none(x => cond)", "no element (satisfies `cond`)"),
    ("find", "find(x => cond)", "the first element that satisfies `cond`, or none"),
    ("single", "single() · single(x => cond)", "the only element (that satisfies `cond`); fails otherwise"),
    ("first", "first() · first(x => cond) · first(n)", "the first element (that satisfies `cond`), or none; `first(n)`: a list of the first n"),
    ("last", "last() · last(x => cond) · last(n)", "the last element (that satisfies `cond`), or none; `last(n)`: a list of the last n"),
    ("contains", "contains(x)", "`x` is an element"),
    ("distinct", "distinct()", "the elements without duplicates, in order"),
    ("sort_by", "sort_by(x => key)", "the elements in ascending order of `key` (a tuple: several keys; negate a number for descending)"),
    ("last_by", "last_by(x => key)", "the element with the greatest `key`, or none"),
    ("max", "max() · max(x => v)", "the greatest value (none skipped), or none"),
    ("min", "min() · min(x => v)", "the smallest value (none skipped), or none"),
    (
        "group_by",
        "group_by(x => key) · group_by(x => key, x => key2)",
        "the groups, in order of their first element: `(key, items)`, or `(key, key2, items)` with two keys. Read them with `g.key`, `g.key2`, `g.items` (`g.items.count()`, `.sum(x => v)`, `.avg`, `.min`, `.max`). Over an entity (`Order.where(…).group_by(o => o.customer.region, o => o.date.month)…map(g => …)`) the database groups the rows (GROUP BY) when the groups are read through their keys and these aggregates",
    ),
    ("allocate", "allocate(total, by=x => weight)", "split an amount in proportion to the weights, to the smallest unit: `(item, share)` tuples"),
    ("is_contiguous", "is_contiguous()", "whole numbers with no gaps and no duplicates"),
];

/// Hover over a method of the language after a `.` (`lines.group_by(…)`): its signature and
/// what it gives.
pub fn method_hover(text: &str, offset: TextSize) -> Option<(TextRange, String)> {
    let at = usize::from(offset).min(text.len());
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let start = text[..at].rfind(|c: char| !ident(c)).map_or(0, |i| i + 1);
    let end = text[at..]
        .find(|c: char| !ident(c))
        .map_or(text.len(), |i| at + i);
    if start >= end || !text[..start].ends_with('.') {
        return None;
    }
    let name = &text[start..end];
    let (_, sig, doc) = METHODS.iter().find(|m| m.0 == name)?;
    Some((
        TextRange::new(TextSize::from(start as u32), TextSize::from(end as u32)),
        format!("{}\n\n{doc}", code(&format!(".{sig}"))),
    ))
}

/// The methods to offer after a `.` at `offset` (with what is typed of the name): (name,
/// signature, documentation). Empty when the position does not follow a `.`.
pub fn method_completions(
    text: &str,
    offset: TextSize,
) -> Vec<(&'static str, &'static str, &'static str)> {
    let at = usize::from(offset).min(text.len());
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let start = text[..at].rfind(|c: char| !ident(c)).map_or(0, |i| i + 1);
    if !text[..start].ends_with('.') {
        return Vec::new();
    }
    let typed = &text[start..at];
    METHODS
        .iter()
        .filter(|m| m.0.starts_with(typed))
        .copied()
        .collect()
}

#[cfg(test)]
mod method_tests {
    use super::*;

    #[test]
    fn methods_after_a_dot() {
        let text = "x = Order.group_by(o => o.customer).map(g => g.items.co";
        let at = text.find("group_by").unwrap() + 3;
        let (range, doc) = method_hover(text, TextSize::from(at as u32)).unwrap();
        assert_eq!(&text[range], "group_by");
        assert!(doc.contains("key2") && doc.contains("GROUP BY"));
        assert!(
            method_hover("group_by", TextSize::from(2)).is_none(),
            "not after a dot"
        );
        let names: Vec<&str> = method_completions(text, TextSize::from(text.len() as u32))
            .iter()
            .map(|m| m.0)
            .collect();
        assert_eq!(names, vec!["count", "contains"]);
    }
}
