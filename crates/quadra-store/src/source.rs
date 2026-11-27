//! Loading on demand from the database, as of a snapshot transaction, with a shared
//! read-only cache.
//!
//! The history is append-only, so a snapshot read is simple: the entries with `tx <= base`.
//! The cache keeps committed records as of its watermark; before serving a newer snapshot it
//! drops the records written since (one indexed query), so it never serves stale data.

use crate::codec::{cause_from_json, from_json};
use crate::conn::{Conn, Dialect, Pooled, Sql};
use crate::schema::{Decode, FieldMap, Part, Schema, SetMap, Source as TableSource, Table};
use quadra_runtime::store::{LedgerRow, Record, Source};
use quadra_runtime::{Id, V};
use serde_json::Value as J;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn num(s: &Option<String>) -> i64 {
    s.as_deref().and_then(|x| x.parse().ok()).unwrap_or(0)
}

fn q(name: &str) -> String {
    format!("\"{name}\"")
}

/// The name a history entry written at `tx` has today, after the renames that followed it.
pub fn renamed(
    renames: &[(String, String, String, u64)],
    ent: &str,
    tx: u64,
    mut field: String,
) -> String {
    for (e, old, new, rtx) in renames {
        if e == ent && *rtx > tx {
            if field == *old {
                field = new.clone();
            } else if field.starts_with(&format!("{old}@")) || field.starts_with(&format!("{old}#"))
            {
                field = format!("{new}{}", &field[old.len()..]);
            }
        }
    }
    field
}

/// Records loaded from the database by this process (benchmarks report them per scenario).
pub static RECORDS_LOADED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Records as of a transaction, in bulk, from the history.
pub fn load_records(
    conn: &mut Conn,
    ids: &[Id],
    base: u64,
    renames: &[(String, String, String, u64)],
) -> Result<Vec<Record>, String> {
    load(conn, ids, base, renames, None, None)
}

/// The columns of `_quadra_records` a record is built from (`r` is the table's alias).
const META: &str = "CAST(r.\"id\" AS TEXT), r.\"entity\", CAST(r.\"created_tx\" AS TEXT), r.\"created_at\", CAST(r.\"created_by\" AS TEXT), CAST(r.\"frozen_tx\" AS TEXT), r.\"cause\", r.\"warnings\", CAST(r.\"last_tx\" AS TEXT), CAST(r.\"version_tx\" AS TEXT), r.\"kinds\"";
const NMETA: usize = 11;

/// What the projection row of an entity gives a record: the columns of its fields (stored
/// ones when its rows read back exactly, `readable`; formulas always), and those of its sets
/// and per-company values, each gathered in one JSON column.
struct RowRead<'s> {
    table: &'s Table,
    fields: Vec<&'s FieldMap>,
    readable: bool,
    sets: Vec<&'s SetMap>,
    companies: Vec<&'s Table>,
}

impl<'s> RowRead<'s> {
    fn of(schema: &'s Schema, ent: &str, rows_exact: bool) -> Option<RowRead<'s>> {
        let table = schema.entity_table(ent)?;
        let readable = schema.readable.contains(ent) && rows_exact;
        // every formula column (the incremental ones are read instead of computed; the others
        // tell a commit whether the value it computes changed)
        let fields: Vec<&FieldMap> = table
            .fields
            .iter()
            .filter(|fm| {
                if fm.formula {
                    fm.decode != Decode::Unknown
                } else {
                    readable
                }
            })
            .collect();
        let (sets, companies) = if readable {
            (
                schema
                    .sets
                    .get(ent)
                    .map(|v| v.iter().collect())
                    .unwrap_or_default(),
                schema.per_company_tables(ent),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        Some(RowRead {
            table,
            fields,
            readable,
            sets,
            companies,
        })
    }

    fn is_empty(&self) -> bool {
        self.fields.is_empty() && self.sets.is_empty() && self.companies.is_empty()
    }

    /// The select list over the row (alias `t`).
    fn columns(&self, d: Dialect) -> Vec<String> {
        let mut out: Vec<String> = self
            .fields
            .iter()
            .flat_map(|fm| {
                fm.columns
                    .iter()
                    .map(|c| format!("CAST(t.{} AS TEXT)", q(&c.name)))
            })
            .collect();
        for sm in &self.sets {
            out.push(match d {
                Dialect::Sqlite => format!("(SELECT json_group_array(json_array(CAST(j.\"target_id\" AS TEXT), CAST(j.\"ord\" AS TEXT))) FROM {} j WHERE j.\"owner_id\" = t.\"id\")", q(&sm.table)),
                Dialect::Postgres => format!("(SELECT CAST(json_agg(json_build_array(CAST(j.\"target_id\" AS TEXT), CAST(j.\"ord\" AS TEXT))) AS TEXT) FROM {} j WHERE j.\"owner_id\" = t.\"id\")", q(&sm.table)),
            });
        }
        for pt in &self.companies {
            let mut parts = vec!["CAST(j.\"company_id\" AS TEXT)".to_string()];
            if let Some(fm) = pt.fields.first() {
                parts.extend(
                    fm.columns
                        .iter()
                        .map(|c| format!("CAST(j.{} AS TEXT)", q(&c.name))),
                );
            }
            out.push(match d {
                Dialect::Sqlite => format!("(SELECT json_group_array(json_array({})) FROM {} j WHERE j.\"owner_id\" = t.\"id\")", parts.join(", "), q(&pt.name)),
                Dialect::Postgres => format!("(SELECT CAST(json_agg(json_build_array({})) AS TEXT) FROM {} j WHERE j.\"owner_id\" = t.\"id\")", parts.join(", "), q(&pt.name)),
            });
        }
        out
    }

    fn width(&self) -> usize {
        self.fields.iter().map(|fm| fm.columns.len()).sum::<usize>()
            + self.sets.len()
            + self.companies.len()
    }

    /// Fill a record from the text of its row's columns: its formulas, and its stored values
    /// when the row reads back exactly and is its last write (`version`). Whether the record
    /// was read from its row (else its history is needed).
    fn apply(
        &self,
        rec: &mut Record,
        vals: &[Option<String>],
        version: u64,
        kinds: Option<&str>,
    ) -> bool {
        let mut at = 0;
        let mut stored: Vec<(String, V)> = Vec::new();
        let mut ok = true;
        // the values of another kind of number than their columns (`db::kinds_of`)
        let kinds: Option<serde_json::Map<String, J>> =
            kinds.and_then(|k| serde_json::from_str(k).ok());
        let kind = |key: &str| {
            kinds
                .as_ref()
                .and_then(|m| m.get(key))
                .and_then(|k| k.as_str())
        };
        for fm in &self.fields {
            let v = &vals[at..at + fm.columns.len()];
            at += fm.columns.len();
            let read = match kind(&fm.field) {
                Some(k) if !fm.formula => decode_kind(k, v),
                _ => decode(fm, v),
            };
            match read {
                Ok(Some(v)) if fm.formula => {
                    rec.formulas.insert(fm.field.clone(), v);
                }
                // no value, or the formula failed (its column is then empty): computed when
                // read, which tells them apart
                Ok(None) if fm.formula => {
                    rec.formulas.insert(fm.field.clone(), V::None);
                }
                Ok(Some(v)) => stored.push((fm.field.clone(), v)),
                Ok(None) => {}
                Err(()) if fm.formula => {}
                Err(()) => ok = false,
            }
        }
        for sm in &self.sets {
            let text = vals[at].as_deref().unwrap_or("[]");
            at += 1;
            // (reference, position): a row without its position (an older version) is not exact
            let pairs: Option<Vec<(Id, i64)>> =
                serde_json::from_str::<Vec<Vec<Option<String>>>>(text)
                    .ok()
                    .and_then(|v| {
                        v.iter()
                            .map(|p| {
                                Some((
                                    p.first()?.as_deref()?.parse::<Id>().ok()?,
                                    p.get(1)?.as_deref()?.parse::<i64>().ok()?,
                                ))
                            })
                            .collect()
                    });
            match pairs {
                // no row: no value (read as the empty set), see `db::fits_refs`
                Some(pairs) if pairs.is_empty() => {}
                Some(mut pairs) => {
                    pairs.sort_unstable_by_key(|(_, n)| *n);
                    let ent: Rc<str> = Rc::from(sm.target.as_str());
                    let items: Vec<V> = pairs
                        .into_iter()
                        .map(|(id, _)| V::Ref {
                            ent: ent.clone(),
                            id,
                        })
                        .collect();
                    stored.push((
                        sm.field.clone(),
                        if sm.set {
                            V::Set(Rc::new(items))
                        } else {
                            V::List(Rc::new(items))
                        },
                    ));
                }
                None => ok = false,
            }
        }
        for pt in &self.companies {
            let text = vals[at].as_deref().unwrap_or("[]");
            at += 1;
            let (Some(fm), Ok(rows)) = (
                pt.fields.first(),
                serde_json::from_str::<Vec<Vec<Option<String>>>>(text),
            ) else {
                ok = false;
                continue;
            };
            for row in rows {
                let Some(company) = row.first().and_then(|c| c.as_deref()) else {
                    ok = false;
                    continue;
                };
                let key = format!("{}@{company}", fm.field);
                let read = match kind(&key) {
                    Some(k) => decode_kind(k, &row[1..]),
                    None => decode(fm, &row[1..]),
                };
                match read {
                    Ok(Some(v)) => stored.push((key, v)),
                    Ok(None) => {}
                    Err(()) => ok = false,
                }
            }
        }
        // the row is the record's last write: its values hold, at `version` (a record with no
        // value at all would not know when it was written)
        if !(self.readable
            && ok
            && version > 0
            && (!stored.is_empty() || version == rec.created_tx))
        {
            return false;
        }
        for (f, val) in stored {
            rec.history.push((version, f.clone(), val.clone()));
            rec.fields.insert(f, val);
        }
        rec.partial = Some(version);
        true
    }
}

/// A record from its row of `_quadra_records` (`META`), without its values.
fn meta_record(r: &[Option<String>], base: u64) -> Record {
    let cause =
        cause_from_json(&serde_json::from_str(r[6].as_deref().unwrap_or("[]")).unwrap_or(J::Null));
    let frozen: Option<u64> = r[5]
        .as_deref()
        .and_then(|s| s.parse().ok())
        .filter(|t| *t <= base);
    let ent = r[1].clone().unwrap_or_default();
    Record {
        ent: Rc::from(ent.as_str()),
        id: num(&r[0]) as u64,
        fields: Default::default(),
        history: Vec::new(),
        created_tx: num(&r[2]) as u64,
        created_at: chrono::NaiveDateTime::parse_from_str(
            r[3].as_deref().unwrap_or(""),
            "%Y-%m-%dT%H:%M:%S",
        )
        .unwrap_or_default(),
        created_by: r[4].as_deref().and_then(|s| s.parse().ok()),
        pending_defaults: Vec::new(),
        frozen_at: frozen,
        cause,
        warnings: serde_json::from_str(r[7].as_deref().unwrap_or("[]")).unwrap_or_default(),
        partial: None,
        formulas: HashMap::new(),
    }
}

/// Records as of a transaction, in bulk: from their projection rows when `schema` says how to
/// read them back and the row has not changed since `base` (`last_tx`), else from the history;
/// with the values of their incremental formulas when the row has not changed since `base`.
/// `hint`: the entity the records are expected to be of: its rows come in the same query as
/// the records (one query instead of two); records of another entity are read as without it.
pub fn load(
    conn: &mut Conn,
    ids: &[Id],
    base: u64,
    renames: &[(String, String, String, u64)],
    schema: Option<&Schema>,
    hint: Option<&str>,
) -> Result<Vec<Record>, String> {
    load_with(conn, ids, base, renames, schema, hint, true)
}

/// `load`; `fields`: false reads the stored values from the history even when the rows hold
/// them exactly (only the formulas come from the rows: `quadra db verify`).
pub fn load_with(
    conn: &mut Conn,
    ids: &[Id],
    base: u64,
    renames: &[(String, String, String, u64)],
    schema: Option<&Schema>,
    hint: Option<&str>,
    fields: bool,
) -> Result<Vec<Record>, String> {
    let d = conn.dialect();
    let mut out: HashMap<Id, Record> = HashMap::new();
    // the hinted entity's row, read with the records (all its columns: whether its rows read
    // back exactly is known per record, `version_tx`)
    let joined = match (schema, hint) {
        (Some(s), Some(h)) => RowRead::of(s, h, fields).filter(|r| !r.is_empty()),
        _ => None,
    };
    for chunk in ids.chunks(400) {
        let list: Vec<String> = chunk.iter().map(|i| i.to_string()).collect();
        let list = list.join(", ");
        // the records, and whether their projection rows are those of the snapshot
        let mut current: HashMap<String, Vec<(Id, u64)>> = HashMap::new();
        let mut kinds: HashMap<Id, String> = HashMap::new();
        let mut from_rows: std::collections::HashSet<Id> = std::collections::HashSet::new();
        // `+`: the records by their ids, not by the index of `created_tx`, which SQLite would
        // otherwise pick and read through every record created before
        let sql = match &joined {
            Some(j) => format!("SELECT {META}, {} FROM \"_quadra_records\" r LEFT JOIN {} t ON t.\"id\" = r.\"id\" WHERE r.\"id\" IN ({list}) AND +r.\"created_tx\" <= ?bigint", j.columns(d).join(", "), q(&j.table.name)),
            None => format!("SELECT {META} FROM \"_quadra_records\" r WHERE r.\"id\" IN ({list}) AND +r.\"created_tx\" <= ?bigint"),
        };
        for r in conn.query(&sql, &[Sql::Int(base as i64)])? {
            let mut rec = meta_record(&r, base);
            let id = rec.id;
            let (last, version) = (num(&r[8]) as u64, num(&r[9]) as u64);
            // the row is the snapshot's: its formulas hold; its fields too when it reads back
            // exactly (`version_tx` > 0, `db::exact_row`)
            if schema.is_some() && last <= base {
                match &joined {
                    Some(j) if hint == Some(&*rec.ent) => {
                        if j.apply(
                            &mut rec,
                            &r[NMETA..NMETA + j.width()],
                            version,
                            r[10].as_deref(),
                        ) {
                            from_rows.insert(id);
                        }
                    }
                    _ => {
                        current
                            .entry(rec.ent.to_string())
                            .or_default()
                            .push((id, version));
                        if let Some(k) = &r[10] {
                            kinds.insert(id, k.clone());
                        }
                    }
                }
            }
            out.insert(id, rec);
        }
        // projection rows of the other entities
        if let Some(schema) = schema {
            let mut ents: Vec<&String> = current.keys().collect();
            ents.sort();
            for ent in ents {
                let rows = &current[ent];
                let Some(rr) = RowRead::of(schema, ent, fields && rows_exact(rows)) else {
                    continue;
                };
                if rr.is_empty() {
                    continue;
                }
                let versions: HashMap<Id, u64> = rows.iter().copied().collect();
                let idl: Vec<String> = rows.iter().map(|(i, _)| i.to_string()).collect();
                let sql = format!(
                    "SELECT CAST(t.\"id\" AS TEXT), {} FROM {} t WHERE t.\"id\" IN ({})",
                    rr.columns(d).join(", "),
                    q(&rr.table.name),
                    idl.join(", ")
                );
                for row in conn.query(&sql, &[])? {
                    let id = num(&row[0]) as u64;
                    let Some(rec) = out.get_mut(&id) else {
                        continue;
                    };
                    if rr.apply(
                        rec,
                        &row[1..],
                        versions[&id],
                        kinds.get(&id).map(String::as_str),
                    ) {
                        from_rows.insert(id);
                    }
                }
            }
        }
        // the history of the others
        let rest: Vec<String> = chunk
            .iter()
            .filter(|i| out.contains_key(i) && !from_rows.contains(i))
            .map(|i| i.to_string())
            .collect();
        if rest.is_empty() {
            continue;
        }
        for r in conn.query(
            &format!(
                "SELECT CAST(\"tx\" AS TEXT), CAST(\"record_id\" AS TEXT), \"field\", \"value\" FROM \"_quadra_history\" WHERE \"record_id\" IN ({}) AND \"tx\" <= ?bigint ORDER BY \"_quadra_history\".\"record_id\", \"_quadra_history\".\"seq\"",
                rest.join(", ")
            ),
            &[Sql::Int(base as i64)],
        )? {
            let id = num(&r[1]) as u64;
            let Some(rec) = out.get_mut(&id) else { continue };
            let tx = num(&r[0]) as u64;
            let field = renamed(renames, &rec.ent, tx, r[2].clone().unwrap_or_default());
            let v = from_json(&serde_json::from_str(r[3].as_deref().unwrap_or("null")).unwrap_or(J::Null));
            rec.fields.insert(field.clone(), v.clone());
            rec.history.push((tx, field, v));
        }
    }
    let mut v: Vec<Record> = out.into_values().collect();
    v.sort_by_key(|r| r.id);
    RECORDS_LOADED.fetch_add(v.len() as u64, std::sync::atomic::Ordering::Relaxed);
    Ok(v)
}

fn rows_exact(rows: &[(Id, u64)]) -> bool {
    rows.iter().any(|(_, v)| *v > 0)
}

/// A value from the text of its columns (`CAST(… AS TEXT)`), as `db::encode` wrote it: None
/// for no value, Err when it cannot be read back exactly.
pub fn decode(fm: &FieldMap, vals: &[Option<String>]) -> Result<Option<V>, ()> {
    use rust_decimal::Decimal;
    use std::str::FromStr;
    let Some(first) = vals.first().ok_or(())? else {
        return Ok(None);
    };
    let dec = |s: &str| Decimal::from_str(s).map_err(|_| ());
    let text = |i: usize| -> Result<Rc<str>, ()> {
        vals.get(i)
            .and_then(|v| v.as_deref())
            .map(Rc::from)
            .ok_or(())
    };
    Ok(Some(match &fm.decode {
        Decode::Unknown => return Err(()),
        Decode::Bool => match first.as_str() {
            "1" | "t" | "true" => V::Bool(true),
            "0" | "f" | "false" => V::Bool(false),
            _ => return Err(()),
        },
        Decode::Int => V::Int(first.parse().map_err(|_| ())?),
        Decode::Dec => V::Dec(dec(first)?),
        Decode::Pct => V::Pct(dec(first)?),
        Decode::Text => V::Text(Rc::from(first.as_str())),
        Decode::Role => V::Role(Rc::from(first.as_str())),
        Decode::Variant => V::Variant {
            name: Rc::from(first.as_str()),
            fields: Rc::new(Vec::new()),
        },
        Decode::YearMonth => {
            let (y, m) = first.split_once('-').ok_or(())?;
            V::YearMonth(y.parse().map_err(|_| ())?, m.parse().map_err(|_| ())?)
        }
        Decode::Date => V::Date(chrono::NaiveDate::from_str(first).map_err(|_| ())?),
        Decode::DateTime => V::DateTime(
            chrono::NaiveDateTime::parse_from_str(first, "%Y-%m-%d %H:%M:%S").map_err(|_| ())?,
        ),
        Decode::Duration => V::Duration(first.parse().map_err(|_| ())?),
        Decode::Money => V::Money {
            amt: dec(first)?,
            cur: text(1)?,
        },
        Decode::Price => V::Price {
            amt: dec(first)?,
            cur: text(1)?,
            per: text(2)?,
        },
        Decode::Qty => V::Qty {
            v: dec(first)?,
            unit: text(1)?,
        },
        Decode::Ref(e) => V::Ref {
            ent: Rc::from(e.as_str()),
            id: first.parse().map_err(|_| ())?,
        },
        Decode::Json => from_json(&serde_json::from_str::<J>(first).map_err(|_| ())?),
    }))
}

/// A number held by columns of another kind (`db::kind_note`): `int`, `dec` or `pct`, from
/// the text of its first column.
fn decode_kind(kind: &str, vals: &[Option<String>]) -> Result<Option<V>, ()> {
    use rust_decimal::prelude::ToPrimitive;
    use std::str::FromStr;
    let Some(first) = vals.first().ok_or(())? else {
        return Ok(None);
    };
    let d = rust_decimal::Decimal::from_str(first).map_err(|_| ())?;
    Ok(Some(match kind {
        "int" if d.fract().is_zero() => V::Int(d.to_i64().ok_or(())?),
        "dec" => V::Dec(d),
        "pct" => V::Pct(d),
        _ => return Err(()),
    }))
}

pub fn load_renames(conn: &mut Conn) -> Result<Vec<(String, String, String, u64)>, String> {
    Ok(conn
        .query("SELECT CAST(\"tx\" AS TEXT), \"entity\", \"old_name\", \"new_name\" FROM \"_quadra_renames\" ORDER BY \"_quadra_renames\".\"seq\"", &[])?
        .into_iter()
        .map(|r| (r[1].clone().unwrap_or_default(), r[2].clone().unwrap_or_default(), r[3].clone().unwrap_or_default(), num(&r[0]) as u64))
        .collect())
}

pub fn decode_row(r: &[Option<String>]) -> LedgerRow {
    let cols: J = serde_json::from_str(r[5].as_deref().unwrap_or("{}")).unwrap_or(J::Null);
    let cols = cols
        .as_object()
        .map(|o| o.iter().map(|(k, v)| (k.clone(), from_json(v))).collect())
        .unwrap_or_default();
    let source =
        from_json(&serde_json::from_str(r[4].as_deref().unwrap_or("null")).unwrap_or(J::Null));
    LedgerRow {
        ledger: Rc::from(r[1].as_deref().unwrap_or("")),
        id: num(&r[0]) as u64,
        cols,
        source: if source.is_none() { None } else { Some(source) },
        date: chrono::NaiveDateTime::parse_from_str(
            r[3].as_deref().unwrap_or(""),
            "%Y-%m-%dT%H:%M:%S",
        )
        .unwrap_or_default(),
        tx: num(&r[2]) as u64,
    }
}

pub const ROW_COLS: &str =
    "CAST(\"id\" AS TEXT), \"ledger\", CAST(\"tx\" AS TEXT), \"date\", \"source\", \"cols\"";

pub fn head(conn: &mut Conn) -> Result<u64, String> {
    let r = conn.query(
        "SELECT CAST(MAX(\"tx\") AS TEXT) FROM \"_quadra_commits\"",
        &[],
    )?;
    Ok(r.first().map(|r| num(&r[0]) as u64).unwrap_or(0))
}

// ---- cache --------------------------------------------------------------------------------

struct Entry {
    rec: Record,
    bytes: usize,
    used: u64,
    loaded: Instant,
}

/// Committed records shared by the transactions of one engine (one thread per tenant).
pub struct Cache {
    /// the records are as of this transaction
    watermark: u64,
    entries: HashMap<Id, Entry>,
    bytes: usize,
    pub max_bytes: usize,
    pub ttl: Duration,
    tick: u64,
    pub hits: u64,
    pub misses: u64,
}

fn size_of(r: &Record) -> usize {
    96 + r
        .history
        .iter()
        .map(|(_, f, v)| 48 + f.len() + v.to_string().len())
        .sum::<usize>()
}

impl Cache {
    pub fn new(max_bytes: usize, ttl: Duration) -> Cache {
        Cache {
            watermark: 0,
            entries: HashMap::new(),
            bytes: 0,
            max_bytes,
            ttl,
            tick: 0,
            hits: 0,
            misses: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Forget every record (and the counters).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
        self.hits = 0;
        self.misses = 0;
    }

    /// Bring the cache to `base`: drop what was written after the watermark. Whether fields
    /// were renamed since (the cache is then emptied).
    pub fn sync(&mut self, conn: &mut Conn, base: u64) -> Result<bool, String> {
        if base <= self.watermark {
            return Ok(false);
        }
        let renamed = conn.query(
            "SELECT '1' FROM \"_quadra_renames\" WHERE \"tx\" > ?bigint LIMIT 1",
            &[Sql::Int(self.watermark as i64)],
        )?;
        let any = !renamed.is_empty();
        if any {
            self.entries.clear();
            self.bytes = 0;
        } else if !self.entries.is_empty() {
            // drop what was written since (an empty cache has nothing to drop: a server's first
            // request does not read the ids of every record written before it)
            for r in conn.query("SELECT DISTINCT CAST(\"id\" AS TEXT) FROM \"_quadra_records\" WHERE \"last_tx\" > ?bigint", &[Sql::Int(self.watermark as i64)])? {
                if let Some(e) = self.entries.remove(&(num(&r[0]) as u64)) {
                    self.bytes -= e.bytes;
                }
            }
        }
        self.watermark = base;
        Ok(any)
    }

    /// The transaction the cached records are as of.
    pub fn watermark(&self) -> u64 {
        self.watermark
    }

    fn get(&mut self, id: Id, base: u64) -> Option<Record> {
        self.tick += 1;
        let tick = self.tick;
        let ttl = self.ttl;
        let expired = self
            .entries
            .get(&id)
            .map_or(false, |e| e.loaded.elapsed() > ttl);
        if expired {
            let e = self.entries.remove(&id).unwrap();
            self.bytes -= e.bytes;
        }
        let e = self.entries.get_mut(&id)?;
        e.used = tick;
        let mut r = e.rec.clone();
        if base < self.watermark {
            // an older snapshot: cut the history (a record read from its projection row has
            // only its values since its last write); its formulas may be newer than the snapshot
            if r.created_tx > base || r.partial.is_some_and(|v| v > base) {
                return None;
            }
            r.formulas.clear();
            r.history.retain(|(t, _, _)| *t <= base);
            r.fields.clear();
            for (_, f, v) in &r.history {
                r.fields.insert(f.clone(), v.clone());
            }
            if r.frozen_at.map_or(false, |t| t > base) {
                r.frozen_at = None;
            }
        }
        self.hits += 1;
        Some(r)
    }

    fn put(&mut self, r: Record, base: u64) {
        if base != self.watermark {
            return;
        }
        let bytes = size_of(&r);
        if bytes > self.max_bytes / 4 {
            return;
        }
        self.tick += 1;
        if let Some(old) = self.entries.insert(
            r.id,
            Entry {
                rec: r,
                bytes,
                used: self.tick,
                loaded: Instant::now(),
            },
        ) {
            self.bytes -= old.bytes;
        }
        self.bytes += bytes;
        if self.bytes > self.max_bytes {
            // least recently used first, down to 90% of the budget
            let mut by_use: Vec<(u64, Id)> =
                self.entries.iter().map(|(id, e)| (e.used, *id)).collect();
            by_use.sort();
            for (_, id) in by_use {
                if self.bytes <= self.max_bytes * 9 / 10 {
                    break;
                }
                if let Some(e) = self.entries.remove(&id) {
                    self.bytes -= e.bytes;
                }
            }
        }
    }

    /// Load the most recent records (created after `since`) up to the byte budget.
    pub fn prewarm(
        &mut self,
        conn: &mut Conn,
        since: chrono::NaiveDateTime,
    ) -> Result<usize, String> {
        let base = head(conn)?;
        self.sync(conn, base)?;
        let renames = load_renames(conn)?;
        let ids: Vec<Id> = conn
            .query("SELECT CAST(\"id\" AS TEXT) FROM \"_quadra_records\" WHERE \"created_at\" >= ?text ORDER BY \"_quadra_records\".\"id\" DESC", &[Sql::Text(since.format("%Y-%m-%dT%H:%M:%S").to_string())])?
            .iter()
            .map(|r| num(&r[0]) as u64)
            .collect();
        let mut n = 0;
        for chunk in ids.chunks(400) {
            for r in load_records(conn, chunk, base, &renames)? {
                self.put(r, base);
                n += 1;
            }
            if self.bytes >= self.max_bytes * 9 / 10 {
                break;
            }
        }
        Ok(n)
    }
}

// ---- source -------------------------------------------------------------------------------

/// The renames of fields recorded in a database: (entity, old name, new name, transaction).
pub type Renames = Vec<(String, String, String, u64)>;

/// Committed data as of `base`, loaded on demand.
pub struct DbSource {
    conn: Pooled,
    base: u64,
    schema: Rc<Schema>,
    renames: Renames,
    cache: Option<Rc<RefCell<Cache>>>,
    error: Option<String>,
    pub queries: u64,
    /// the connection is inside a transaction of its owner (a deploy): ids are reserved in
    /// it, without a transaction of their own
    pub in_tx: bool,
    /// queries over whole entities may be answered in SQL (off: the interpreter scans, the
    /// reference the tests compare SQL with)
    pub sql: bool,
    /// the project's sequencer, when it runs: it gives the ids (no write here)
    pub sequencer: Option<crate::Sequencer>,
    /// records are read from their projection rows when they can be, with the values of their
    /// incremental formulas (off: from the history, every formula computed: the reference)
    pub rows: bool,
    /// with `rows`: their stored values too (off: from the history, only the formulas from the
    /// rows; the ledger tables are not read either: `quadra db verify` checks the rows against
    /// the history)
    pub fields_from_rows: bool,
    /// (entity, field) → records whose row may not say what their history says of the field
    /// (`quadra db verify` found them): candidates of every lookup of the field by value, which
    /// the store then checks on the record
    pub patch: HashMap<(String, String), Vec<Id>>,
    /// with `rows` and `fields_from_rows` off: the stored values of the records are read from
    /// their rows all the same, but for those of `history_ids` (`quadra db verify`: the rows it
    /// checked, the records of the page being checked and those whose rows were found wrong
    /// read from their history). The ledger tables are still not read.
    pub rows_trusted: bool,
    pub history_ids: std::collections::HashSet<Id>,
}

impl DbSource {
    /// A snapshot at the current head of the database.
    pub fn open(
        url: &str,
        schema: Rc<Schema>,
        cache: Option<Rc<RefCell<Cache>>>,
    ) -> Result<DbSource, String> {
        let mut conn = Conn::open(url)?;
        let base = head(&mut conn)?;
        DbSource::at(Pooled::own(conn), base, schema, cache)
    }

    pub fn at(
        mut conn: Pooled,
        base: u64,
        schema: Rc<Schema>,
        cache: Option<Rc<RefCell<Cache>>>,
    ) -> Result<DbSource, String> {
        let renames = load_renames(&mut conn)?;
        DbSource::with_renames(conn, base, schema, cache, renames)
    }

    /// Like `at`, with the renames already known (they change only with a migration: an
    /// engine keeps them).
    pub fn with_renames(
        mut conn: Pooled,
        base: u64,
        schema: Rc<Schema>,
        cache: Option<Rc<RefCell<Cache>>>,
        renames: Renames,
    ) -> Result<DbSource, String> {
        if let Some(c) = &cache {
            c.borrow_mut().sync(&mut conn, base)?;
        }
        Ok(DbSource {
            conn,
            base,
            schema,
            renames,
            cache,
            error: None,
            queries: 0,
            in_tx: false,
            sql: true,
            sequencer: None,
            rows: std::env::var("QUADRA_NO_ROWS").is_err(),
            fields_from_rows: true,
            patch: HashMap::new(),
            rows_trusted: false,
            history_ids: Default::default(),
        })
    }

    /// The connection, for its owner's own statements (a deploy writes through it).
    pub fn conn(&mut self) -> &mut Conn {
        &mut self.conn
    }

    /// Exchange the connection with another one (a deploy reads and writes through the same
    /// connection, inside its transaction).
    pub fn swap_conn(&mut self, other: &mut Pooled) {
        std::mem::swap(&mut self.conn, other);
    }

    pub fn renames(&self) -> &Renames {
        &self.renames
    }

    fn note<T>(&mut self, r: Result<T, String>) -> Option<T> {
        self.queries += 1;
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                self.error.get_or_insert(e);
                None
            }
        }
    }

    /// `find` in the projection tables.
    fn find_rows(&mut self, ent: &str, field: &str, value: &V) -> Option<Vec<Id>> {
        // a set of references holding the record: its join table
        if let V::Ref { id, .. } = value {
            let jt = self
                .schema
                .tables
                .iter()
                .find(|t| matches!(&t.source, TableSource::Join(e, f) if e == ent && f == field))
                .map(|t| t.name.clone());
            if let Some(jt) = jt {
                let r = self.conn.query(&format!("SELECT CAST(\"owner_id\" AS TEXT) FROM {} WHERE \"target_id\" = ?bigint ORDER BY \"owner_id\"", q(&jt)), &[Sql::Int(*id as i64)]);
                return Some(self.note(r)?.iter().map(|r| num(&r[0]) as u64).collect());
            }
        }
        let (table, cond, v) = self.column(ent, field, value)?;
        let r = self.conn.query(
            &format!(
                "SELECT CAST(\"id\" AS TEXT) FROM {} WHERE {cond}",
                q(&table)
            ),
            &[v],
        );
        Some(self.note(r)?.iter().map(|r| num(&r[0]) as u64).collect())
    }

    /// Records from the cache, the others loaded (`hint`: the entity they are expected to be
    /// of, see `load`).
    fn load_cached(&mut self, ids: &[Id], hint: Option<&str>) -> Vec<Record> {
        let mut out = Vec::new();
        let mut missing = Vec::new();
        // records whose stored values come from their history whatever their rows say (never
        // from the cache, which holds them as read from their rows)
        let mut history = Vec::new();
        for id in ids {
            if self.rows_trusted && self.history_ids.contains(id) {
                history.push(*id);
                continue;
            }
            match self
                .cache
                .as_ref()
                .and_then(|c| c.borrow_mut().get(*id, self.base))
            {
                Some(r) => out.push(r),
                None => missing.push(*id),
            }
        }
        let base = self.base;
        let schema = self.schema.clone();
        let rows = (self.rows && !self.in_tx).then_some(&*schema);
        if !missing.is_empty() {
            let r = load_with(
                &mut self.conn,
                &missing,
                base,
                &self.renames,
                rows,
                hint,
                self.fields_from_rows || self.rows_trusted,
            );
            for r in self.note(r).unwrap_or_default() {
                if let Some(c) = &self.cache {
                    let mut c = c.borrow_mut();
                    c.misses += 1;
                    c.put(r.clone(), base);
                }
                out.push(r);
            }
        }
        if !history.is_empty() {
            let r = load_with(
                &mut self.conn,
                &history,
                base,
                &self.renames,
                rows,
                hint,
                false,
            );
            out.extend(self.note(r).unwrap_or_default());
        }
        out
    }

    /// The single column holding `field` of an entity or ledger, and how to compare a value.
    fn column(&self, ent: &str, field: &str, value: &V) -> Option<(String, String, Sql)> {
        use crate::schema::ColType;
        let t = self.schema.tables.iter().find(
            |t| matches!(&t.source, TableSource::Entity(e) | TableSource::Ledger(e) if e == ent),
        )?;
        let fm = t.fields.iter().find(|f| f.field == field && !f.formula)?;
        let c = fm.columns.first()?;
        let num = |d: rust_decimal::Decimal| Sql::Text(d.normalize().to_string());
        let col = q(&c.name);
        // the condition on the column, as SQL compares it (decimals are text on SQLite)
        let (cond, sql) = match (c.part, c.ty, value) {
            (Part::RefId, _, V::Ref { id, .. }) => {
                (format!("{col} = ?bigint"), Sql::Int(*id as i64))
            }
            (Part::Scalar, ColType::Text, V::Text(s)) => {
                (format!("{col} = ?text"), Sql::Text(s.to_string()))
            }
            (Part::Scalar, ColType::Text, V::Variant { name, fields }) if fields.is_empty() => {
                (format!("{col} = ?text"), Sql::Text(name.to_string()))
            }
            (Part::Scalar, ColType::BigInt, V::Int(i)) => {
                (format!("{col} = ?bigint"), Sql::Int(*i))
            }
            (Part::Duration, _, V::Duration(s)) => (format!("{col} = ?bigint"), Sql::Int(*s)),
            (Part::Scalar, ColType::Bool, V::Bool(b)) => {
                (format!("{col} = ?boolean"), Sql::Bool(*b))
            }
            (Part::Scalar, ColType::Date, V::Date(d)) => {
                (format!("{col} = ?date"), Sql::Text(d.to_string()))
            }
            (Part::Scalar, ColType::Timestamp, V::DateTime(d)) => (
                format!("{col} = ?timestamp"),
                Sql::Text(d.format("%Y-%m-%d %H:%M:%S").to_string()),
            ),
            // a number, or the amount of a money / quantity / price (the caller checks the rest)
            (Part::Scalar | Part::Amount, ColType::Numeric, v) => {
                let d = match v {
                    V::Money { amt, .. } | V::Price { amt, .. } => *amt,
                    V::Qty { v, .. } => *v,
                    x => x.num()?,
                };
                match self.conn.dialect() {
                    Dialect::Postgres => (format!("{col} = ?numeric"), num(d)),
                    Dialect::Sqlite => (format!("{col} COLLATE quadra_dec = ?text"), num(d)),
                }
            }
            _ => return None,
        };
        Some((t.name.clone(), cond, sql))
    }
}

impl Source for DbSource {
    fn base(&self) -> u64 {
        self.base
    }

    fn record(&mut self, id: Id) -> Option<Record> {
        self.load_cached(&[id], None).into_iter().next()
    }

    fn records(&mut self, ids: &[Id]) -> Vec<Record> {
        self.load_cached(ids, None)
    }

    fn record_of(&mut self, ent: &str, id: Id) -> Option<Record> {
        self.load_cached(&[id], Some(ent)).into_iter().next()
    }

    fn records_of(&mut self, ent: &str, ids: &[Id]) -> Vec<Record> {
        self.load_cached(ids, Some(ent))
    }

    fn wants_hints(&self) -> bool {
        true
    }

    fn full_record(&mut self, id: Id) -> Option<Record> {
        let base = self.base;
        let r = load_records(&mut self.conn, &[id], base, &self.renames);
        self.note(r)?.into_iter().next()
    }

    fn ids(&mut self, ent: &str) -> Vec<Id> {
        let r = self
            .conn
            .query("SELECT CAST(\"id\" AS TEXT) FROM \"_quadra_records\" WHERE \"entity\" = ?text AND \"created_tx\" <= ?bigint ORDER BY \"_quadra_records\".\"id\"", &[Sql::Text(ent.into()), Sql::Int(self.base as i64)]);
        self.note(r)
            .unwrap_or_default()
            .iter()
            .map(|r| num(&r[0]) as u64)
            .collect()
    }

    fn find(&mut self, ent: &str, field: &str, value: &V) -> Option<Vec<Id>> {
        let mut out = self.find_rows(ent, field, value)?;
        if let Some(more) = self.patch.get(&(ent.to_string(), field.to_string())) {
            out.extend(more.iter().copied());
            out.sort();
            out.dedup();
        }
        Some(out)
    }

    fn find_many(
        &mut self,
        ent: &str,
        field: &str,
        targets: &[Id],
    ) -> Option<HashMap<Id, Vec<Id>>> {
        let jt = self
            .schema
            .tables
            .iter()
            .find(|t| matches!(&t.source, TableSource::Join(e, f) if e == ent && f == field))
            .map(|t| t.name.clone());
        let (table, key, col) = match jt {
            Some(jt) => (jt, "owner_id".to_string(), "target_id".to_string()),
            None => {
                let t = self
                    .schema
                    .tables
                    .iter()
                    .find(|t| matches!(&t.source, TableSource::Entity(e) if e == ent))?;
                let fm = t.fields.iter().find(|f| {
                    f.field == field
                        && !f.formula
                        && f.columns.len() == 1
                        && f.columns[0].part == Part::RefId
                })?;
                (t.name.clone(), "id".to_string(), fm.columns[0].name.clone())
            }
        };
        let mut out: HashMap<Id, Vec<Id>> = HashMap::new();
        if let Some(more) = self
            .patch
            .get(&(ent.to_string(), field.to_string()))
            .cloned()
        {
            for t in targets {
                out.entry(*t).or_default().extend(more.iter().copied());
            }
        }
        for chunk in targets.chunks(400) {
            let marks = vec!["?bigint"; chunk.len()].join(", ");
            let params: Vec<Sql> = chunk.iter().map(|t| Sql::Int(*t as i64)).collect();
            let r = self.conn.query(&format!("SELECT CAST({} AS TEXT), CAST({} AS TEXT) FROM {} WHERE {} IN ({marks}) ORDER BY {}", q(&key), q(&col), q(&table), q(&col), q(&key)), &params);
            for row in self.note(r)? {
                out.entry(num(&row[1]) as u64)
                    .or_default()
                    .push(num(&row[0]) as u64);
            }
        }
        if self
            .patch
            .contains_key(&(ent.to_string(), field.to_string()))
        {
            for ids in out.values_mut() {
                ids.sort();
                ids.dedup();
            }
        }
        Some(out)
    }

    fn rows(&mut self, ledger: &str, filter: Option<(&str, &V)>) -> Vec<LedgerRow> {
        let base = self.base as i64;
        // (the ledger tables are not trusted: every row of the ledger, the store filters them)
        let filter = filter.filter(|_| self.fields_from_rows);
        if let Some((col, v)) = filter {
            if let Some((table, cond, sql)) = self.column(ledger, col, v) {
                let r = self.conn.query(
                    &format!(
                        "SELECT CAST(\"id\" AS TEXT) FROM {} WHERE {cond} AND \"_tx\" <= ?bigint",
                        q(&table)
                    ),
                    &[sql, Sql::Int(base)],
                );
                let ids: Vec<String> = self
                    .note(r)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|r| r[0].clone())
                    .collect();
                let mut out = Vec::new();
                for chunk in ids.chunks(400) {
                    let r = self.conn.query(&format!("SELECT {ROW_COLS} FROM \"_quadra_ledger_rows\" WHERE \"id\" IN ({}) ORDER BY \"_quadra_ledger_rows\".\"id\"", chunk.join(", ")), &[]);
                    out.extend(
                        self.note(r)
                            .unwrap_or_default()
                            .iter()
                            .map(|r| decode_row(r)),
                    );
                }
                return out;
            }
        }
        let r =
            self.conn.query(&format!("SELECT {ROW_COLS} FROM \"_quadra_ledger_rows\" WHERE \"ledger\" = ?text AND \"tx\" <= ?bigint ORDER BY \"_quadra_ledger_rows\".\"id\""), &[Sql::Text(ledger.into()), Sql::Int(base)]);
        self.note(r)
            .unwrap_or_default()
            .iter()
            .map(|r| decode_row(r))
            .collect()
    }

    fn row(&mut self, id: Id) -> Option<LedgerRow> {
        let r = self.conn.query(&format!("SELECT {ROW_COLS} FROM \"_quadra_ledger_rows\" WHERE \"id\" = ?bigint AND \"tx\" <= ?bigint"), &[Sql::Int(id as i64), Sql::Int(self.base as i64)]);
        self.note(r)?.first().map(|r| decode_row(r))
    }

    fn postings(&mut self, record: Id) -> HashMap<String, Vec<HashMap<String, V>>> {
        let r = self.conn.query("SELECT \"ledger_key\", \"rows\" FROM \"_quadra_postings\" WHERE \"record_id\" = ?bigint", &[Sql::Int(record as i64)]);
        let mut out = HashMap::new();
        for r in self.note(r).unwrap_or_default() {
            let rows: J = serde_json::from_str(r[1].as_deref().unwrap_or("[]")).unwrap_or(J::Null);
            let rows = rows
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|o| {
                            o.as_object()
                                .map(|o| o.iter().map(|(k, v)| (k.clone(), from_json(v))).collect())
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.insert(r[0].clone().unwrap_or_default(), rows);
        }
        out
    }

    fn counter(&mut self, key: &str) -> u64 {
        let r = self.conn.query(
            "SELECT CAST(\"n\" AS TEXT) FROM \"_quadra_counters\" WHERE \"key\" = ?text",
            &[Sql::Text(key.into())],
        );
        self.note(r)
            .and_then(|r| r.first().map(|r| num(&r[0]) as u64))
            .unwrap_or(0)
    }

    fn commit(&mut self, tx: u64) -> Option<(chrono::NaiveDateTime, Option<Id>)> {
        let r = self.conn.query("SELECT \"at\", CAST(\"user_id\" AS TEXT) FROM \"_quadra_commits\" WHERE \"tx\" = ?bigint", &[Sql::Int(tx as i64)]);
        let row = self.note(r)?.into_iter().next()?;
        let at = chrono::NaiveDateTime::parse_from_str(
            row[0].as_deref().unwrap_or(""),
            "%Y-%m-%dT%H:%M:%S",
        )
        .ok()?;
        Some((at, row[1].as_deref().and_then(|u| u.parse().ok())))
    }

    fn reserve_ids(&mut self, n: u64) -> Id {
        let r = match &self.sequencer {
            Some(s) if !self.in_tx => s.ids(n),
            _ if self.in_tx => reserve_in(&mut self.conn, n),
            _ => reserve_ids(&mut self.conn, n),
        };
        self.note(r).unwrap_or(0)
    }

    fn error(&mut self) -> Option<String> {
        self.error.clone()
    }

    fn select(
        &mut self,
        r: &quadra_runtime::query::Request,
    ) -> Result<quadra_runtime::query::Answer, String> {
        // a deploy reads and writes the projections in its own transaction: not settled yet
        if self.in_tx || !self.sql {
            return Err("no SQL here".into());
        }
        let d = self.conn.dialect();
        let plan = crate::sqlq::plan(d, &self.schema, r)?;
        self.queries += 1;
        plan.run_counted(&mut self.conn, r)
    }

    fn search(
        &mut self,
        ent: &str,
        fields: &[String],
        q: &str,
        words: Option<&quadra_runtime::query::TextSearch>,
        after: Option<&quadra_runtime::query::After>,
        limit: usize,
        perm: Option<(
            &quadra_ir::Model,
            &quadra_runtime::query::Perm,
            &quadra_runtime::query::Frees,
        )>,
    ) -> Result<quadra_runtime::query::Searched, String> {
        if !self.sql {
            return Err("no SQL here".into());
        }
        let d = self.conn.dialect();
        let (f, perm_sql) =
            crate::sqlq::search_sql(d, &self.schema, ent, fields, q, words, after, limit, perm)?;
        self.queries += 1;
        let rows = self.conn.query(&f.sql, &f.params)?;
        let rows = rows
            .into_iter()
            .map(|r| {
                let id: Id = r[0].as_deref().and_then(|x| x.parse().ok()).unwrap_or(0);
                (
                    id,
                    quadra_runtime::query::After {
                        keys: vec![r[1].clone(), r[2].clone()],
                        id,
                    },
                    r[3].as_deref() != Some("0"),
                )
            })
            .collect();
        Ok(quadra_runtime::query::Searched { rows, perm_sql })
    }

    fn changed_since(&mut self, entities: &[String]) -> Result<bool, String> {
        if entities.is_empty() {
            return Ok(false);
        }
        // the records written after the snapshot (an index range on `last_tx`: none when the
        // snapshot is the latest commit), then whether one is of these entities
        self.queries += 1;
        let sql = match self.conn.dialect() {
            // SQLite would rather walk the entity index for the DISTINCT
            Dialect::Sqlite => "SELECT \"entity\" FROM \"_quadra_records\" INDEXED BY \"qx__quadra_records_last_tx\" WHERE \"last_tx\" > ?bigint LIMIT 10001",
            Dialect::Postgres => "SELECT \"entity\" FROM \"_quadra_records\" WHERE \"last_tx\" > ?bigint LIMIT 10001",
        };
        let r = self.conn.query(sql, &[Sql::Int(self.base as i64)])?;
        // many writes since: the interpreter answers (whatever they touched)
        if r.len() > 10_000 {
            return Ok(true);
        }
        Ok(r.iter().any(|row| {
            row[0]
                .as_deref()
                .map_or(false, |e| entities.iter().any(|x| x == e))
        }))
    }

    fn rename(&mut self, ent: &str, old: &str, new: &str, tx: u64) -> usize {
        // the names the history may hold for `old` today (earlier renames to it included)
        let mut names: Vec<String> = vec![old.to_string()];
        loop {
            let more: Vec<String> = self
                .renames
                .iter()
                .filter(|(e, o, n, _)| e == ent && names.contains(n) && !names.contains(o))
                .map(|(_, o, _, _)| o.clone())
                .collect();
            if more.is_empty() {
                break;
            }
            names.extend(more);
        }
        let mut cond = Vec::new();
        let mut params = vec![Sql::Text(ent.to_string())];
        for n in &names {
            cond.push(
                "h.\"field\" = ?text OR h.\"field\" LIKE ?text OR h.\"field\" LIKE ?text"
                    .to_string(),
            );
            params.extend([
                Sql::Text(n.clone()),
                Sql::Text(format!("{n}@%")),
                Sql::Text(format!("{n}#%")),
            ]);
        }
        params.push(Sql::Int(self.base as i64));
        let sql = format!(
            "SELECT CAST(h.\"record_id\" AS TEXT), CAST(h.\"tx\" AS TEXT), h.\"field\" FROM \"_quadra_history\" h JOIN \"_quadra_records\" r ON r.\"id\" = h.\"record_id\" WHERE r.\"entity\" = ?text AND ({}) AND h.\"tx\" <= ?bigint",
            cond.join(" OR ")
        );
        let rows = self.conn.query(&sql, &params);
        let rows = self.note(rows).unwrap_or_default();
        let mut keys: std::collections::HashSet<(i64, String)> = std::collections::HashSet::new();
        for r in rows {
            let key = renamed(
                &self.renames,
                ent,
                num(&r[1]) as u64,
                r[2].clone().unwrap_or_default(),
            );
            if key == old
                || key.starts_with(&format!("{old}@"))
                || key.starts_with(&format!("{old}#"))
            {
                keys.insert((num(&r[0]), key));
            }
        }
        self.renames
            .push((ent.to_string(), old.to_string(), new.to_string(), tx));
        keys.len()
    }
}

/// Reserve `n` ids inside the caller's transaction (which holds the lock); returns the first.
fn reserve_in(conn: &mut Conn, n: u64) -> Result<Id, String> {
    let get = |conn: &mut Conn, k: &str| -> Result<u64, String> {
        let r = conn.query(
            "SELECT CAST(\"value\" AS TEXT) FROM \"_quadra_meta\" WHERE \"key\" = ?text",
            &[Sql::Text(k.into())],
        )?;
        Ok(r.first()
            .and_then(|r| r[0].clone())
            .map(|v| {
                if k == "state" {
                    serde_json::from_str::<J>(&v)
                        .ok()
                        .and_then(|s| s["next_id"].as_u64())
                        .unwrap_or(0)
                } else {
                    v.parse().unwrap_or(0)
                }
            })
            .unwrap_or(0))
    };
    let from = get(conn, "id_reserved")?.max(get(conn, "state")?);
    conn.exec("INSERT INTO \"_quadra_meta\" (\"key\", \"value\") VALUES ('id_reserved', ?text) ON CONFLICT (\"key\") DO UPDATE SET \"value\" = excluded.\"value\"", &[Sql::Text((from + n).to_string())])?;
    Ok(from + 1)
}

/// Reserve `n` ids in the database (its own short transaction); returns the first.
pub fn reserve_ids(conn: &mut Conn, n: u64) -> Result<Id, String> {
    let begin = if conn.dialect() == Dialect::Sqlite {
        "BEGIN IMMEDIATE"
    } else {
        "BEGIN"
    };
    conn.batch(begin)?;
    let r = (|| -> Result<Id, String> {
        if conn.dialect() == Dialect::Postgres {
            conn.batch("LOCK TABLE \"_quadra_meta\" IN EXCLUSIVE MODE")?;
        }
        let get = |conn: &mut Conn, k: &str| -> Result<u64, String> {
            let r = conn.query(
                "SELECT CAST(\"value\" AS TEXT) FROM \"_quadra_meta\" WHERE \"key\" = ?text",
                &[Sql::Text(k.into())],
            )?;
            Ok(r.first()
                .and_then(|r| r[0].clone())
                .map(|v| {
                    if k == "state" {
                        serde_json::from_str::<J>(&v)
                            .ok()
                            .and_then(|s| s["next_id"].as_u64())
                            .unwrap_or(0)
                    } else {
                        v.parse().unwrap_or(0)
                    }
                })
                .unwrap_or(0))
        };
        let from = get(conn, "id_reserved")?.max(get(conn, "state")?);
        let to = from + n;
        conn.exec("INSERT INTO \"_quadra_meta\" (\"key\", \"value\") VALUES ('id_reserved', ?text) ON CONFLICT (\"key\") DO UPDATE SET \"value\" = excluded.\"value\"", &[Sql::Text(to.to_string())])?;
        Ok(from + 1)
    })();
    match r {
        Ok(v) => {
            conn.batch("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.batch("ROLLBACK");
            Err(e)
        }
    }
}
