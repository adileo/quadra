//! `quadra trace Entity.field`: everything that can change a value, and everything it feeds.

use quadra_ir::{print, FieldKind, Model, Span};
use quadra_sema::Project;
use quadra_types::{Node, Report};
use std::collections::{BTreeMap, BTreeSet, HashSet};

fn location(project: &Project, span: Span) -> String {
    let f = project.file(span.file);
    let line = f.text[..usize::from(span.range.start()).min(f.text.len())]
        .matches('\n')
        .count()
        + 1;
    let rel = f.path.strip_prefix(&project.root).unwrap_or(&f.path);
    format!("{}:{line}", rel.display())
}

fn label(n: &Node) -> String {
    if n.0 == "metric" {
        format!("metric {}", n.1)
    } else if n.1 == "*" {
        format!("{} (ledger)", n.0)
    } else {
        format!("{}.{}", n.0, n.1.trim_end_matches("#default"))
    }
}

fn describe(model: &Model, n: &Node) -> String {
    if n.1 == "*" {
        let posters: Vec<String> = model
            .entities
            .iter()
            .flat_map(|e| {
                e.posts
                    .iter()
                    .filter(|p| p.ledger == n.0)
                    .map(move |p| match &p.when {
                        Some(w) => format!("{} when {}", e.name, print::expr(w)),
                        None => e.name.clone(),
                    })
            })
            .collect();
        return format!("← posted by {}", posters.join(", "));
    }
    if n.0 == "metric" {
        return String::new();
    }
    let Some(f) = model
        .entity(&n.0)
        .and_then(|e| model.find_field(e, n.1.trim_end_matches("#default")))
    else {
        return "built-in".into();
    };
    match (&f.formula, &f.default, f.kind) {
        (Some(x), _, _) => format!("= {}", print::expr(x)),
        (_, _, FieldKind::LedgerKey | FieldKind::LedgerMeasure | FieldKind::LedgerAttr) => {
            "ledger column".into()
        }
        (_, Some(d), _) => format!("stored · default {}", print::expr(d)),
        _ => "stored".into(),
    }
}

fn is_derived(model: &Model, n: &Node) -> bool {
    n.1 == "*"
        || n.0 == "metric"
        || model
            .entity(&n.0)
            .and_then(|e| model.find_field(e, &n.1))
            .map_or(false, |f| f.formula.is_some())
}

pub fn trace(
    project: &Project,
    model: &Model,
    report: &Report,
    target: &str,
) -> Result<String, String> {
    let (ent, field) = target
        .split_once('.')
        .ok_or_else(|| format!("expected Entity.field, got `{target}`"))?;
    let e = model
        .entity(ent)
        .ok_or_else(|| format!("unknown entity `{ent}`"))?;
    let f = model
        .find_field(e, field)
        .ok_or_else(|| format!("`{ent}` has no field `{field}`"))?;
    let root: Node = (ent.to_string(), field.to_string());
    let mut out = String::new();

    let kind = match f.kind {
        FieldKind::Formula => "formula",
        FieldKind::Stored => "stored field",
        _ => "ledger column",
    };
    let ty =
        f.ty.as_ref()
            .map(print::ty)
            .unwrap_or_else(|| "(inferred)".into());
    out.push_str(&format!(
        "{target} : {ty}    {kind} · {}\n",
        location(project, f.span)
    ));
    if let Some(x) = &f.formula {
        out.push_str(&format!("  = {}\n", print::expr(x)));
    }
    if let Some(d) = &f.default {
        out.push_str(&format!("  default {}\n", print::expr(d)));
    }
    if f.per_company {
        out.push_str("  per company\n");
    }

    // constraints
    let mut constraints = Vec::new();
    if let Some(w) = &f.where_ {
        constraints.push(match w {
            quadra_ir::Where::Compare(op, x) => {
                format!("where {} {}", print::binop(*op), print::expr(x))
            }
            quadra_ir::Where::Expr(x) => format!("where {}", print::expr(x)),
        });
    }
    if let Some(r) = &f.required_when {
        constraints.push(format!("required when {}", print::expr(r)));
    }
    let word = |s: &str| {
        s.split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|w| w == field)
    };
    for c in &e.checks {
        let text = print::expr(&c.cond);
        if word(&text) {
            constraints.push(format!(
                "{} {}    {}",
                if c.warn { "warn" } else { "check" },
                text,
                location(project, c.span)
            ));
        }
    }
    // the lines of a frozen document are frozen with it
    if let Some((owner, fr)) = model
        .frozen_owner(e)
        .and_then(|o| o.frozen.as_ref().map(|fr| (o, fr)))
    {
        let live = e
            .frozen
            .as_ref()
            .map_or(false, |own| own.live.iter().any(|l| l == field))
            || e.live_decls.iter().any(|(l, _)| l == field);
        let when = if owner.name == e.name {
            format!("frozen when {}", print::expr(&fr.when))
        } else {
            format!(
                "frozen with its {} (frozen when {})",
                owner.name,
                print::expr(&fr.when)
            )
        };
        constraints.push(format!(
            "{when}{}",
            if live {
                "  (this formula stays live)"
            } else {
                ""
            }
        ));
    }
    if !constraints.is_empty() {
        out.push_str("\nConstraints\n");
        for c in constraints {
            out.push_str(&format!("  {c}\n"));
        }
    }

    // dependency tree
    let mut stored: BTreeSet<Node> = BTreeSet::new();
    if is_derived(model, &root) {
        out.push_str("\nDepends on\n");
        let mut seen: HashSet<Node> = HashSet::new();
        seen.insert(root.clone());
        tree(model, report, &root, 1, &mut seen, &mut stored, &mut out);
    } else {
        stored.insert(root.clone());
    }

    // writers of the stored fields underneath
    if !stored.is_empty() {
        out.push_str("\nStored values underneath, and who writes them\n");
        for n in &stored {
            let mut writers: BTreeMap<String, String> = BTreeMap::new();
            for (ctx, span) in report.writes.get(n).map(Vec::as_slice).unwrap_or(&[]) {
                if ctx.starts_with("test ")
                    || ctx.starts_with("fixture ")
                    || ctx == "seed"
                    || ctx.is_empty()
                {
                    continue;
                }
                writers
                    .entry(ctx.clone())
                    .or_insert_with(|| location(project, *span));
            }
            let flow_governed = model
                .entity(&n.0)
                .map_or(false, |e| e.flows.iter().any(|fl| fl.field == n.1));
            let ledger = model.entity(&n.0).map_or(false, |e| e.is_ledger);
            out.push_str(&format!("  {}\n", label(n)));
            if !flow_governed && !ledger {
                out.push_str("      user, through forms and API\n");
            }
            for (ctx, loc) in writers {
                out.push_str(&format!("      {ctx:<44} {loc}\n"));
            }
        }
    }

    // readers
    let mut readers: Vec<String> = report
        .reads
        .iter()
        .filter(|(from, tos)| *from != &root && tos.iter().any(|(t, _)| *t == root))
        .map(|(from, _)| {
            if from.1.ends_with("#default") {
                format!("{} (default)", label(from))
            } else {
                label(from)
            }
        })
        .collect();
    readers.sort();
    readers.dedup();
    if !readers.is_empty() {
        out.push_str("\nRead by\n");
        for r in readers {
            out.push_str(&format!("  {r}\n"));
        }
    }
    Ok(out)
}

fn tree(
    model: &Model,
    report: &Report,
    n: &Node,
    depth: usize,
    seen: &mut HashSet<Node>,
    stored: &mut BTreeSet<Node>,
    out: &mut String,
) {
    let mut kids: Vec<Node> = report
        .reads
        .get(n)
        .map(|v| v.iter().map(|(t, _)| t.clone()).collect())
        .unwrap_or_default();
    kids.sort();
    kids.dedup();
    for k in kids {
        if k.1 == "*" && k.0 == n.0 {
            continue;
        }
        let indent = "  ".repeat(depth);
        let derived = is_derived(model, &k);
        if !derived {
            stored.insert(k.clone());
        }
        if !seen.insert(k.clone()) {
            if derived {
                out.push_str(&format!("{indent}{}  (see above)\n", label(&k)));
            }
            continue;
        }
        out.push_str(&format!("{indent}{}  {}\n", label(&k), describe(model, &k)));
        if derived && depth < 8 {
            tree(model, report, &k, depth + 1, seen, stored, out);
        }
    }
}
