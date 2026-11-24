// The data model as a graph, for the "Model" view of the code editor: entities, records,
// traits, ledgers and enums with their fields and relations, of the code being edited (unsaved
// buffers included), and what changes against the model running.

use super::*;
use quadra_ir::{EntityKind, FieldKind, Model, Span, StmtKind};
use quadra_types::Report;
use std::collections::BTreeMap;

/// One side of the comparison.
struct Side<'a> {
    model: &'a Model,
    report: &'a Report,
}

impl Side<'_> {
    fn type_of(&self, ent: &str, f: &quadra_ir::Field) -> String {
        match self.report.field_types.get(&(ent.to_string(), f.name.clone())) {
            Some(t) => t.to_string(),
            None => f.ty.as_ref().map(quadra_ir::print::ty).unwrap_or_default(),
        }
    }

    /// The entity or enum a type points to: `Partner`, `Partner?`, `[SalesLine]`, `{Tag}`.
    fn target(&self, ty: &str) -> Option<(String, bool)> {
        let many = ty.starts_with('[') || ty.starts_with('{');
        let name: String = ty.trim_start_matches(['[', '{']).chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        (self.model.entity(&name).is_some() || self.model.enum_def(&name).is_some()).then_some((name, many))
    }
}

fn entity_kind(e: &quadra_ir::Entity) -> &'static str {
    if e.temporary {
        "temporary"
    } else if e.is_ledger {
        "ledger"
    } else {
        match e.kind {
            EntityKind::Entity => "entity",
            EntityKind::Record => "record",
            EntityKind::Trait => "trait",
        }
    }
}

fn field_kind(f: &quadra_ir::Field) -> &'static str {
    match f.kind {
        FieldKind::Stored => "stored",
        FieldKind::Formula => "formula",
        FieldKind::LedgerKey => "key",
        FieldKind::LedgerMeasure => "measure",
        FieldKind::LedgerAttr => "attr",
    }
}

/// `{entity: {field: (type, kind)}}`: what the comparison looks at.
type Shape = BTreeMap<String, (String, BTreeMap<String, (String, String)>)>;

fn shape(side: &Side) -> Shape {
    let mut out = Shape::new();
    for e in side.model.entities.iter().filter(|e| !e.name.starts_with("__")) {
        let fields = side.model.all_fields(e).into_iter().filter(|f| !f.name.starts_with("__")).map(|f| (f.name.clone(), (side.type_of(&e.name, f), field_kind(f).to_string()))).collect();
        out.insert(e.name.clone(), (entity_kind(e).to_string(), fields));
    }
    for en in &side.model.enums {
        let variants = en.variants.iter().map(|v| (v.name.clone(), (String::new(), "variant".to_string()))).collect();
        out.insert(en.name.clone(), ("enum".into(), variants));
    }
    out
}

/// `rename E.old -> E.new` in the migrations that the running model has not got yet.
fn pending_renames(current: &Model, deployed: &Model) -> Vec<(String, String, String, String)> {
    let mut out = Vec::new();
    for m in current.migrations.iter().filter(|m| !deployed.migrations.iter().any(|d| d.name == m.name)) {
        for s in &m.body {
            if let StmtKind::Migration { keyword, paths, .. } = &s.kind {
                if let (true, Some(from), Some(to)) = (keyword == "rename", paths.first(), paths.get(1)) {
                    if from.len() >= 2 && !to.is_empty() {
                        out.push((from[0].clone(), from[from.len() - 1].clone(), to[to.len() - 1].clone(), m.name.clone()));
                    }
                }
            }
        }
    }
    out
}

/// What changes from `old` to `new`: entities and fields added, removed, renamed (by a
/// pending migration), with another type or kind.
fn diff(old: &Shape, new: &Shape, renames: &[(String, String, String, String)]) -> J {
    let mut entities = Vec::new();
    for (name, (kind, fields)) in new {
        let Some((okind, ofields)) = old.get(name) else {
            entities.push(json!({ "name": name, "change": "added", "kind": kind }));
            continue;
        };
        let mut changes = Vec::new();
        let renamed: Vec<_> = renames.iter().filter(|r| r.0 == *name).collect();
        for (f, (ty, fk)) in fields {
            if let Some(r) = renamed.iter().find(|r| r.2 == *f && ofields.contains_key(&r.1) && !fields.contains_key(&r.1)) {
                let (oty, _) = &ofields[&r.1];
                changes.push(json!({ "field": f, "change": "renamed", "from": r.1, "migration": r.3, "type": ty, "old_type": (oty != ty).then_some(oty) }));
                continue;
            }
            match ofields.get(f) {
                None => changes.push(json!({ "field": f, "change": "added", "type": ty, "kind": fk })),
                Some((oty, ofk)) if oty != ty || ofk != fk => changes.push(json!({ "field": f, "change": "changed", "type": ty, "old_type": oty, "kind": fk, "old_kind": ofk })),
                _ => {}
            }
        }
        for (f, (ty, fk)) in ofields {
            if fields.contains_key(f) || renamed.iter().any(|r| r.1 == *f && fields.contains_key(&r.2)) {
                continue;
            }
            // removed, and a new field of the same type appeared: likely a rename without its migration
            let maybe: Vec<&String> = fields.iter().filter(|(n, (t, k))| !ofields.contains_key(*n) && t == ty && k == fk && !renamed.iter().any(|r| r.2 == **n)).map(|(n, _)| n).collect();
            changes.push(json!({ "field": f, "change": "removed", "type": ty, "kind": fk, "maybe_renamed_to": maybe }));
        }
        if okind != kind || !changes.is_empty() {
            entities.push(json!({ "name": name, "change": "changed", "kind": kind, "old_kind": (okind != kind).then_some(okind), "fields": changes }));
        }
    }
    for (name, (kind, fields)) in old {
        if !new.contains_key(name) {
            let fs: Vec<J> = fields.iter().map(|(f, (ty, fk))| json!({ "name": f, "type": ty, "kind": fk })).collect();
            entities.push(json!({ "name": name, "change": "removed", "kind": kind, "fields": fs }));
        }
    }
    json!(entities)
}

impl Engine {
    pub(super) fn dev_schema(&mut self, who: &Principal, body: &J) -> Result<Reply, Fail> {
        self.editor_login(who)?;
        let state = self.ide_state(&body["buffers"])?;
        // while the code has errors: the last version that compiled, said so
        let (shown, stale) = match state.checked.model {
            Some(_) => (state.clone(), false),
            None => (self.types_for(&state), true),
        };
        let Some(model) = shown.checked.model.as_ref() else {
            return Ok(reply(200, json!({ "entities": [], "enums": [], "stale": true, "diff": [] })));
        };
        let side = Side { model, report: &shown.checked.report };
        let place = |span: &Span| -> J {
            let f = shown.project.file(span.file);
            if f.kind == quadra_sema::FileKind::Std {
                return J::Null;
            }
            let rel = f.path.strip_prefix(&self.root).unwrap_or(&f.path).to_string_lossy().replace('\\', "/");
            let at = |b: usize| f.text[..b.min(f.text.len())].encode_utf16().count();
            json!({ "path": rel, "from": at(usize::from(span.range.start())), "to": at(usize::from(span.range.end())) })
        };
        let module = |span: &Span| shown.project.file(span.file).module.clone();
        let mut entities = Vec::new();
        for e in model.entities.iter().filter(|e| !e.name.starts_with("__")) {
            let own: Vec<&str> = e.fields.iter().map(|f| f.name.as_str()).collect();
            let fields: Vec<J> = model
                .all_fields(e)
                .into_iter()
                .filter(|f| !f.name.starts_with("__"))
                .map(|f| {
                    let ty = side.type_of(&e.name, f);
                    let target = side.target(&ty);
                    json!({
                        "name": f.name,
                        "type": ty,
                        "kind": field_kind(f),
                        "target": target.as_ref().map(|t| &t.0),
                        "many": target.as_ref().map_or(false, |t| t.1),
                        "optional": ty.ends_with('?'),
                        "unique": f.unique,
                        "parent": f.parent,
                        "via": f.via,
                        "inherited": !own.contains(&f.name.as_str()),
                        "extension": f.extension,
                        "place": place(&f.span),
                    })
                })
                .collect();
            entities.push(json!({
                "name": e.name,
                "kind": entity_kind(e),
                "module": module(&e.span),
                "traits": e.traits,
                "fields": fields,
                "place": place(&e.span),
            }));
        }
        let enums: Vec<J> = model.enums.iter().map(|en| json!({ "name": en.name, "module": module(&en.span), "variants": en.variants.iter().map(|v| &v.name).collect::<Vec<_>>(), "place": place(&en.span) })).collect();
        let deployed = self.checked.clone();
        let diff = match deployed.model.as_ref() {
            Some(d) => {
                let old = shape(&Side { model: d, report: &deployed.report });
                super::schema::diff(&old, &shape(&side), &pending_renames(model, d))
            }
            None => json!([]),
        };
        Ok(reply(200, json!({ "entities": entities, "enums": enums, "stale": stale, "diff": diff })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[(&str, &[(&str, &str)])]) -> Shape {
        items.iter().map(|(e, fs)| (e.to_string(), ("entity".to_string(), fs.iter().map(|(f, t)| (f.to_string(), (t.to_string(), "stored".to_string()))).collect()))).collect()
    }

    #[test]
    fn the_diff_names_renames_and_suspects_missing_migrations() {
        let old = s(&[("Order", &[("number", "Text"), ("note", "Text"), ("qty", "Number")]), ("Gone", &[("a", "Text")])]);
        let new = s(&[("Order", &[("number", "Code"), ("remark", "Text"), ("amount", "Number")]), ("Fresh", &[("b", "Text")])]);
        let d = diff(&old, &new, &[("Order".into(), "qty".into(), "amount".into(), "m1".into())]);
        let by = |n: &str| d.as_array().unwrap().iter().find(|e| e["name"] == json!(n)).unwrap().clone();
        assert_eq!(by("Fresh")["change"], json!("added"));
        assert_eq!(by("Gone")["change"], json!("removed"));
        let o = by("Order");
        let f = |n: &str| o["fields"].as_array().unwrap().iter().find(|c| c["field"] == json!(n)).unwrap().clone();
        assert_eq!(f("amount")["change"], json!("renamed"));
        assert_eq!(f("amount")["from"], json!("qty"));
        assert_eq!(f("number")["change"], json!("changed"));
        assert_eq!(f("number")["old_type"], json!("Text"));
        assert_eq!(f("remark")["change"], json!("added"));
        assert_eq!(f("note")["change"], json!("removed"));
        assert_eq!(f("note")["maybe_renamed_to"], json!(["remark"]));
        assert!(o["fields"].as_array().unwrap().iter().all(|c| c["field"] != json!("qty")), "{o}");
    }
}
