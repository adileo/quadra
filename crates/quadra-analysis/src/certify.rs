//! Fingerprints of certified metrics and controls, and `metrics.lock`.
//!
//! A fingerprint is the hash of the canonical text of the metric and of everything it
//! depends on, transitively, down to stored fields: their types, formulas, defaults,
//! constraints. Formatting and comments do not change it; any change of meaning does.

use quadra_ir::{print, Model};
use quadra_sema::{Diagnostic, Project, Severity};
use quadra_types::{Node, Report};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};

pub const LOCK_FILE: &str = "metrics.lock";

#[derive(Clone, Debug, PartialEq)]
pub struct LockEntry {
    pub name: String,
    pub fingerprint: String,
    pub dependencies: usize,
    pub approved_by: String,
    pub approved_on: String,
}

/// Canonical text of a node (field, ledger or metric).
fn canonical(model: &Model, node: &Node) -> String {
    if node.0 == "metric" {
        let Some(m) = model.metrics.iter().find(|m| m.name == node.1) else { return String::new() };
        let params: Vec<String> =
            m.params.iter().map(|p| format!("{}: {}", p.name, p.ty.as_ref().map(print::ty).unwrap_or_default())).collect();
        let body: Vec<String> = m.formulas.iter().map(|(k, e)| format!("{k} = {}", print::expr(e))).collect();
        return format!("metric {}({}) -> {}: {}", m.name, params.join(", "), m.ret.as_ref().map(print::ty).unwrap_or_default(), body.join("; "));
    }
    let Some(e) = model.entity(&node.0) else { return String::new() };
    if node.1 == "*" {
        // a ledger: its columns and everything posted to it
        let cols: Vec<String> = e.fields.iter().map(|f| format!("{}: {}", f.name, f.ty.as_ref().map(print::ty).unwrap_or_default())).collect();
        let mut posts = Vec::new();
        for src in &model.entities {
            for p in src.posts.iter().filter(|p| p.ledger == e.name) {
                let when = p.when.as_ref().map(print::expr).unwrap_or_default();
                let body: Vec<String> = p.body.iter().map(print::stmt).collect();
                posts.push(format!("{} when {when}: {}", src.name, body.join("; ")));
            }
        }
        return format!("ledger {}({}) <- {}", e.name, cols.join(", "), posts.join(" | "));
    }
    let name = node.1.trim_end_matches("#default");
    let Some(f) = model.find_field(e, name) else { return String::new() };
    let mut s = format!("{}.{}", e.name, f.name);
    if let Some(t) = &f.ty {
        s.push_str(&format!(": {}", print::ty(t)));
    }
    if let Some(d) = &f.default {
        s.push_str(&format!(" default {}", print::expr(d)));
    }
    if let Some(x) = &f.formula {
        s.push_str(&format!(" = {}", print::expr(x)));
    }
    if f.per_company {
        s.push_str(" per company");
    }
    s
}

/// (name, fingerprint, number of dependencies) for every metric and control.
pub fn fingerprints(model: &Model, report: &Report) -> Vec<(String, String, usize)> {
    let mut out = Vec::new();
    for m in &model.metrics {
        let root: Node = ("metric".into(), m.name.clone());
        let mut seen: BTreeSet<Node> = BTreeSet::new();
        let mut stack = vec![root.clone()];
        while let Some(n) = stack.pop() {
            if !seen.insert(n.clone()) {
                continue;
            }
            // a field's default counts as part of its definition
            let mut next: Vec<Node> = report.reads.get(&n).map(|v| v.iter().map(|(t, _)| t.clone()).collect()).unwrap_or_default();
            if !n.1.ends_with("#default") && n.1 != "*" {
                let d = (n.0.clone(), format!("{}#default", n.1));
                if let Some(v) = report.reads.get(&d) {
                    next.extend(v.iter().map(|(t, _)| t.clone()));
                }
            }
            stack.extend(next);
        }
        let mut hasher = Sha256::new();
        for n in &seen {
            hasher.update(canonical(model, n).as_bytes());
            hasher.update(b"\n");
        }
        let hash = hasher.finalize();
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        out.push((m.name.clone(), format!("sha256:{}", &hex[..16]), seen.len() - 1));
    }
    out
}

pub fn read_lock(text: &str) -> Vec<LockEntry> {
    let mut out: Vec<LockEntry> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            out.push(LockEntry {
                name: line[1..line.len() - 1].to_string(),
                fingerprint: String::new(),
                dependencies: 0,
                approved_by: String::new(),
                approved_on: String::new(),
            });
            continue;
        }
        let (Some(entry), Some((k, v))) = (out.last_mut(), line.split_once('=')) else { continue };
        let v = v.trim().trim_matches('"').to_string();
        match k.trim() {
            "fingerprint" => entry.fingerprint = v,
            "dependencies" => entry.dependencies = v.parse().unwrap_or(0),
            "approved_by" => entry.approved_by = v,
            "approved_on" => entry.approved_on = v,
            _ => {}
        }
    }
    out
}

pub fn write_lock(entries: &[LockEntry]) -> String {
    let mut s = String::from(
        "# Generated by `quadra certify`. Do not edit by hand.\n\
         # Each metric and control is bound to the fingerprint of its complete definition: its\n\
         # formula and everything it depends on, down to stored fields. If the fingerprint\n\
         # computed from the code differs, `quadra check` fails until it is approved again.\n",
    );
    for e in entries {
        s.push_str(&format!(
            "\n[{}]\nfingerprint = \"{}\"\ndependencies = {}\napproved_by = \"{}\"\napproved_on = \"{}\"\n",
            e.name, e.fingerprint, e.dependencies, e.approved_by, e.approved_on
        ));
    }
    s
}

/// Compare the fingerprints of the code with `metrics.lock`, if the project has one.
pub fn verify_lock(project: &Project, model: &Model, report: &Report) -> Vec<Diagnostic> {
    let Ok(text) = std::fs::read_to_string(project.root.join(LOCK_FILE)) else { return Vec::new() };
    let lock: HashMap<String, LockEntry> = read_lock(&text).into_iter().map(|e| (e.name.clone(), e)).collect();
    let mut out = Vec::new();
    for (name, fp, _) in fingerprints(model, report) {
        let Some(m) = model.metrics.iter().find(|m| m.name == name) else { continue };
        let (severity, message) = match lock.get(&name) {
            None => (Severity::Warning, format!("`{name}` is not certified: run `quadra certify {name}`")),
            Some(e) if e.fingerprint != fp => (
                Severity::Error,
                format!(
                    "the definition of `{name}` changed since it was certified by {} on {} ({} → {}): \
                     run `quadra certify {name}` once the change is approved",
                    e.approved_by, e.approved_on, e.fingerprint, fp
                ),
            ),
            Some(_) => continue,
        };
        out.push(Diagnostic { file: m.span.file, range: m.span.range, severity, message });
    }
    out
}
