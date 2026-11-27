//! Phase 4: whole-program analyses on top of the IR and the type checker.
//!
//! - formula cycles (with well-founded recursion over acyclic data);
//! - flows: states that can never be reached;
//! - `trace`: what a field depends on, who writes it, who reads it;
//! - fingerprints of certified metrics and verification against `metrics.lock`;
//! - `fmt`: conservative source formatting.

mod certify;
pub mod clock;
pub mod deps;
mod format;
mod trace;

pub use certify::{fingerprints, read_lock, verify_lock, write_lock, LockEntry};
pub use format::format_source;
pub use trace::trace;

use quadra_ir::{EntityKind, FieldKind, Model};
use quadra_sema::{Diagnostic, Project, Severity};
use quadra_types::{Node, Report};
use std::collections::{HashMap, HashSet};

/// Result of a full check: name analysis, type checking and analyses.
pub struct Checked {
    pub model: Option<Model>,
    pub report: Report,
    pub diagnostics: Vec<Diagnostic>,
    /// the interface files (`interfaces/**/*.qdi`) checked with the model: their problems are
    /// in `diagnostics` too (none when the project has no interface file)
    pub interfaces: Option<quadra_qdi::check::Interfaces>,
}

/// Run every check on a project.
pub fn check(project: &Project) -> Checked {
    let analysis = project.analyze();
    let mut diagnostics = analysis.diagnostics.clone();
    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
        // the interface files' own problems (their names and types need a model)
        let interfaces = interfaces(project, None, &mut diagnostics);
        return Checked { model: None, report: Report::default(), diagnostics, interfaces };
    }
    let mut model = quadra_ir::lower(project, &analysis);
    let report = quadra_types::analyze_model(&model);
    model.zeros = zeros(&model, &report);
    diagnostics.extend(report.diagnostics.iter().cloned());
    diagnostics.extend(cycles(&model, &report));
    // warnings about a declaration point at its first line
    let mut declarations = flows(&model);
    declarations.extend(frozen_reads(&model));
    declarations.extend(verify_lock(project, &model, &report));
    for d in declarations.iter_mut() {
        let text = &project.file(d.file).text;
        let (start, end) = (usize::from(d.range.start()), usize::from(d.range.end()).min(text.len()));
        if let Some(nl) = text.get(start..end).and_then(|t| t.find('\n')) {
            let line_end = start + text[start..start + nl].trim_end().len();
            d.range = quadra_syntax::TextRange::new(d.range.start(), (line_end as u32).into());
        }
    }
    diagnostics.extend(declarations);
    diagnostics.extend(reactions(&model, &report));
    let interfaces = interfaces(project, Some(&model), &mut diagnostics);
    diagnostics.sort_by_key(|d| (d.file, d.range.start()));
    Checked { model: Some(model), report, diagnostics, interfaces }
}

/// The interface files of the project checked with its model, their problems added.
fn interfaces(project: &Project, model: Option<&Model>, diagnostics: &mut Vec<Diagnostic>) -> Option<quadra_qdi::check::Interfaces> {
    project.interface_files().next()?;
    let i = quadra_qdi::check::check(project, model);
    diagnostics.extend(i.diagnostics.iter().cloned());
    Some(i)
}

/// The typed zeros the engine uses where a program computes a bare `0`: fields (stored,
/// defaults, formulas) and `sum(…)` calls of amounts, quantities and prices.
fn zeros(model: &Model, report: &Report) -> HashMap<quadra_ir::Span, quadra_ir::Zero> {
    use quadra_ir::{UnitRef, Zero, ZeroKind};
    use quadra_types::{Unit, T};
    let zero = |t: &T| -> Option<Zero> {
        let unit = |u: &Unit| match u {
            Unit::Const(c) => Some(UnitRef::Code(c.clone())),
            Unit::Path(p) if !p.is_empty() && p[0] != "owner" => Some(UnitRef::Path(p.clone())),
            _ => None,
        };
        let (kind, units) = match t.unopt() {
            T::Money { cur, .. } => (ZeroKind::Money, vec![unit(cur)?]),
            T::Qty { unit: u, .. } => (ZeroKind::Qty, vec![unit(u)?]),
            T::Price { cur, per, .. } => (ZeroKind::Price, vec![unit(cur)?, unit(per)?]),
            _ => return None,
        };
        Some(Zero { kind, units })
    };
    let mut out = HashMap::new();
    for e in &model.entities {
        for f in &e.fields {
            if let Some(z) = report.field_types.get(&(e.name.clone(), f.name.clone())).and_then(|t| zero(t)) {
                out.insert(f.span, z);
            }
        }
    }
    for (span, t) in &report.sum_types {
        if let Some(z) = zero(t) {
            out.insert(*span, z);
        }
    }
    out
}

fn is_formula(model: &Model, node: &Node) -> bool {
    if node.1 == "*" || node.0 == "metric" {
        return true;
    }
    model
        .entity(&node.0)
        .and_then(|e| model.find_field(e, &node.1))
        .map_or(false, |f| f.kind == FieldKind::Formula)
}

/// Cycles among formulas, ledgers and metrics. `default` values do not take part: a
/// snapshot breaks the cycle. A cycle is accepted when one of its entities has a `check`
/// using `closure(...)`, which guarantees the data it recurses on is acyclic.
pub fn cycles(model: &Model, report: &Report) -> Vec<Diagnostic> {
    let mut graph: HashMap<&Node, Vec<&Node>> = HashMap::new();
    for (from, tos) in &report.reads {
        if !is_formula(model, from) {
            continue;
        }
        for (to, _) in tos {
            if is_formula(model, to) {
                graph.entry(from).or_default().push(to);
            }
        }
    }
    let well_founded = |entity: &str| {
        model.entity(entity).map_or(false, |e| {
            e.checks.iter().any(|c| quadra_ir::print::expr(&c.cond).contains("closure("))
        })
    };
    let mut out = Vec::new();
    let mut reported: HashSet<Vec<Node>> = HashSet::new();
    let mut nodes: Vec<&Node> = graph.keys().copied().collect();
    nodes.sort();
    for start in nodes {
        // DFS for a path back to `start`
        let mut stack: Vec<(&Node, Vec<&Node>)> = vec![(start, vec![start])];
        let mut seen: HashSet<&Node> = HashSet::new();
        while let Some((n, path)) = stack.pop() {
            for &next in graph.get(n).map(Vec::as_slice).unwrap_or(&[]) {
                if next == start {
                    let mut cycle: Vec<Node> = path.iter().map(|x| (*x).clone()).collect();
                    if cycle.iter().any(|c| well_founded(&c.0)) {
                        continue;
                    }
                    let mut key = cycle.clone();
                    key.sort();
                    if !reported.insert(key) {
                        continue;
                    }
                    cycle.push(start.clone());
                    let text: Vec<String> = cycle.iter().map(|(e, f)| if f == "*" { e.clone() } else { format!("{e}.{f}") }).collect();
                    let span = report
                        .reads
                        .get(start)
                        .and_then(|v| v.iter().find(|(t, _)| Some(t) == cycle.get(1)))
                        .map(|(_, s)| *s);
                    if let Some(span) = span {
                        out.push(Diagnostic {
                            file: span.file,
                            range: span.range,
                            severity: Severity::Error,
                            message: format!(
                                "formula cycle: {} (break it with a `default` snapshot, or make one value stored)",
                                text.join(" → ")
                            ),
                        });
                    }
                } else if seen.insert(next) {
                    let mut p = path.clone();
                    p.push(next);
                    stack.push((next, p));
                }
            }
        }
    }
    out
}

/// States of a flow that no transition can reach from the initial state.
pub fn flows(model: &Model) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for e in &model.entities {
        if e.kind == EntityKind::Trait {
            continue;
        }
        for flow in &e.flows {
            let Some(field) = model.find_field(e, &flow.field) else { continue };
            let Some(variants) = field.ty.as_ref().and_then(quadra_ir::inline_enum) else { continue };
            let initial = field.default.as_ref().map(quadra_ir::print::expr).unwrap_or_else(|| variants[0].name.clone());
            let mut reached: HashSet<String> = HashSet::from([initial]);
            loop {
                let before = reached.len();
                for t in &flow.transitions {
                    if t.from.iter().any(|f| reached.contains(f)) {
                        reached.insert(t.to.clone());
                    }
                }
                if reached.len() == before {
                    break;
                }
            }
            for v in variants {
                if !reached.contains(&v.name) {
                    out.push(Diagnostic {
                        file: flow.span.file,
                        range: flow.span.range,
                        severity: Severity::Warning,
                        message: format!("state `{}` of `{}.{}` can never be reached", v.name, e.name, flow.field),
                    });
                }
            }
        }
    }
    out
}

/// On a frozen record formulas read the data as of the moment it was frozen, except the
/// `live` ones; the lines of a frozen document (records with a `parent` field) are frozen
/// with it. A formula that gathers records pointing to this one (invoices of an order,
/// payments of an invoice, the stock moves of an order line) would stop seeing the ones
/// created later: almost always a mistake, so it is said, with the fix.
pub fn frozen_reads(model: &Model) -> Vec<Diagnostic> {
    use deps::Step;
    let mut out = Vec::new();
    for e in &model.entities {
        if e.kind == quadra_ir::EntityKind::Trait {
            continue;
        }
        let Some(owner) = model.frozen_owner(e) else { continue };
        let Some(fr) = &owner.frozen else { continue };
        if frozen_at_the_end(owner, fr) {
            continue;
        }
        let line = owner.name != e.name;
        for f in model.all_fields(e) {
            let Some(formula) = &f.formula else { continue };
            let live = e.frozen.as_ref().map_or(false, |own| own.live.contains(&f.name) || own.except.contains(&f.name))
                || e.live_decls.iter().any(|(n, _)| n == &f.name);
            if live {
                continue;
            }
            let d = deps::analyze(model, &e.name, &[formula], &[]);
            // records that point to this one or to its own lines, and are not its own lines
            // (the data reached through other references, rates, accounts: as it was, rightly)
            let mut later: Vec<String> = Vec::new();
            for (path, _, _) in &d.reads {
                for s in path {
                    let Step::Inv { child, back } = s else { break };
                    let own = model.entity(child).and_then(|c| model.find_field(c, back)).map_or(false, |b| b.parent);
                    if !own {
                        later.push(format!("{child} (via {back})"));
                        break;
                    }
                }
            }
            later.sort();
            later.dedup();
            if later.is_empty() {
                continue;
            }
            let frozen = if line { format!("its {} is frozen", owner.name) } else { "it is frozen".to_string() };
            let on = if line { format!("a line of a frozen {}", owner.name) } else { format!("a frozen {}", e.name) };
            out.push(Diagnostic {
                file: f.span.file,
                range: f.span.range,
                severity: Severity::Warning,
                message: format!(
                    "`{}` reads {}: records that point to the {} and come after {frozen}. On {on} it would keep the value it had when frozen; if it should follow them, declare `live {}`{}",
                    f.name,
                    later.join(", "),
                    e.name,
                    f.name,
                    if f.extension { " in this `extend`" } else { "" }
                ),
            });
        }
    }
    out
}

/// Frozen only in final states (`frozen when state in {Done, Cancelled}` with no transition
/// out of them): nothing happens to it afterwards, the snapshot is what is wanted.
fn frozen_at_the_end(e: &quadra_ir::Entity, fr: &quadra_ir::Frozen) -> bool {
    let text = quadra_ir::print::expr(&fr.when);
    let words: Vec<&str> = text.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| !w.is_empty()).collect();
    if words.iter().any(|w| matches!(*w, "not" | "or")) {
        return false;
    }
    let Some(flow) = e.flows.iter().find(|f| words.contains(&f.field.as_str())) else { return false };
    let states: HashSet<&str> = flow.transitions.iter().flat_map(|t| t.from.iter().map(String::as_str).chain(std::iter::once(t.to.as_str()))).collect();
    let named: Vec<&str> = words.iter().copied().filter(|w| states.contains(w)).collect();
    !named.is_empty() && named.iter().all(|s| !flow.transitions.iter().any(|t| t.from.iter().any(|f| f == s || f == "*")))
}

/// A `becomes` reaction whose own writes change what its condition reads can trigger itself
/// again: it must say so with `converges` (each run brings the condition back to false; the
/// engine runs it at most once per record and transaction). `converges` belongs to `becomes`
/// reactions only.
pub fn reactions(model: &Model, report: &Report) -> Vec<Diagnostic> {
    use quadra_ir::Trigger;
    let mut out = Vec::new();
    let related = |a: &str, b: &str| {
        a == b
            || model.entity(a).map_or(false, |e| model.implements(e, b))
            || model.entity(b).map_or(false, |e| model.implements(e, a))
    };
    for r in &model.reactions {
        let Trigger::Becomes(cond) = &r.trigger else {
            if r.converges {
                out.push(Diagnostic {
                    file: r.span.file,
                    range: r.span.range,
                    severity: Severity::Error,
                    message: "`converges` applies only to `becomes` reactions (`on X becomes condition as x converges:`)".into(),
                });
            }
            continue;
        };
        if r.converges {
            continue;
        }
        let d = deps::analyze(model, &r.entity, &[cond], &[]);
        if d.opaque {
            continue;
        }
        // what the reaction itself writes (assignments and fields of records it creates)
        let mut hits: Vec<(&quadra_types::Node, quadra_ir::Span)> = Vec::new();
        for (node, places) in &report.writes {
            for (_, span) in places {
                if span.file != r.span.file || !r.span.range.contains_range(span.range) {
                    continue;
                }
                let read = d.globals.iter().any(|g| related(g, &node.0))
                    || d.reads.iter().any(|(_, ent, field)| *field == node.1 && related(ent, &node.0));
                if read {
                    hits.push((node, *span));
                }
            }
        }
        hits.sort_by_key(|(_, s)| s.range.start());
        let Some((_, first)) = hits.first() else { continue };
        let mut names: Vec<String> = hits.iter().map(|(n, _)| format!("`{}.{}`", n.0, n.1)).collect();
        names.dedup();
        out.push(Diagnostic {
            file: first.file,
            range: first.range,
            severity: Severity::Error,
            message: format!(
                "`on {} becomes {}` can trigger itself again: it writes {}, which its condition reads. If each run brings the condition back to false, declare it `converges` (`as {} converges:`)",
                r.entity,
                quadra_ir::print::expr(cond),
                names.join(", "),
                r.binding
            ),
        });
    }
    out
}

/// Whether something of the model changes with time alone: a formula, a check or a
/// `becomes` reaction that reads the clock (`today()`, `now()`). Such a project needs the
/// periodic sweep, and a server that keeps it asleep must wake it for that (a project that
/// does not read the clock changes only when someone writes).
pub fn reads_clock(model: &Model) -> bool {
    let formulas = model.entities.iter().flat_map(|e| model.all_fields(e).into_iter().filter_map(move |f| f.formula.as_ref().map(|x| (e, x))));
    let checks = model.entities.iter().flat_map(|e| e.checks.iter().map(move |c| (e, &c.cond)));
    for (e, x) in formulas.chain(checks) {
        if deps::analyze(model, &e.name, &[x], &[]).time {
            return true;
        }
    }
    model.reactions.iter().any(|r| match &r.trigger {
        quadra_ir::Trigger::Becomes(cond) => deps::analyze(model, &r.entity, &[cond], &[]).time,
        _ => false,
    })
}
