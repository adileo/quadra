//! SQL schema generated from the IR.
//!
//! - one table per entity (snake_case), with real foreign keys, `CHECK` on enums, `UNIQUE`;
//! - one table per ledger;
//! - one table per trait that is referenced, holding (id, entity) of every implementor, so
//!   that a reference to a trait is a real foreign key;
//! - a join table per stored many-to-many set, a table per `per company` field;
//! - materialized formulas are ordinary (nullable) columns.
//!
//! Money is stored as amount + currency, prices as amount + currency + unit, quantities as
//! value + unit: the table alone is self-explanatory.

use crate::conn::Dialect;
use quadra_ir::{inline_enum, BinOp, EntityKind, Expr, ExprKind, Field, FieldKind, Model, Ty};
use quadra_types::{Num, Report, T};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColType {
    BigInt,
    Text,
    Numeric,
    Bool,
    Date,
    Timestamp,
    Json,
}

impl ColType {
    pub fn sql(self, d: Dialect) -> &'static str {
        match (self, d) {
            (ColType::BigInt, _) => "BIGINT",
            (ColType::Text, _) => "TEXT",
            (ColType::Numeric, Dialect::Sqlite) => "TEXT",
            (ColType::Numeric, Dialect::Postgres) => "NUMERIC",
            (ColType::Bool, Dialect::Sqlite) => "INTEGER",
            (ColType::Bool, Dialect::Postgres) => "BOOLEAN",
            (ColType::Date, Dialect::Sqlite) => "TEXT",
            (ColType::Date, Dialect::Postgres) => "DATE",
            (ColType::Timestamp, Dialect::Sqlite) => "TEXT",
            (ColType::Timestamp, Dialect::Postgres) => "TIMESTAMP",
            (ColType::Json, Dialect::Sqlite) => "TEXT",
            (ColType::Json, Dialect::Postgres) => "JSONB",
        }
    }

    /// Placeholder kind for `conn` (`?numeric`…).
    pub fn param(self) -> &'static str {
        match self {
            ColType::BigInt => "?bigint",
            ColType::Text => "?text",
            ColType::Numeric => "?numeric",
            ColType::Bool => "?boolean",
            ColType::Date => "?date",
            ColType::Timestamp => "?timestamp",
            ColType::Json => "?jsonb",
        }
    }
}

/// Which part of a value a column holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Part {
    Scalar,
    Amount,
    Currency,
    Per,
    Unit,
    RefId,
    Duration,
    Json,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub ty: ColType,
    pub part: Part,
    pub not_null: bool,
    pub check: Option<String>,
    pub references: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FieldMap {
    pub field: String,
    pub formula: bool,
    pub columns: Vec<Column>,
    /// how the columns read back as a value (`Unknown`: they do not: the value is read from
    /// the history)
    #[serde(default)]
    pub decode: Decode,
    /// a materialized formula kept up to date by every commit and read back from its column
    /// (`quadra_analysis::deps::incremental`); false for the others, which are computed
    #[serde(default)]
    pub incremental: bool,
}

/// The value a field's columns hold, to read it back from a projection row exactly as the
/// history has it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decode {
    #[default]
    Unknown,
    Bool,
    Int,
    Dec,
    Pct,
    Text,
    YearMonth,
    Role,
    Variant,
    Date,
    DateTime,
    Duration,
    Money,
    Price,
    Qty,
    /// a reference to a record of this entity
    Ref(String),
    /// the JSON encoding of the history
    Json,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Source {
    Entity(String),
    Ledger(String),
    Trait(String),
    /// stored set of references: (entity, field)
    Join(String, String),
    /// per-company field: (entity, field)
    PerCompany(String, String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Table {
    pub name: String,
    pub source: Source,
    pub columns: Vec<Column>,
    pub primary: Vec<String>,
    pub uniques: Vec<Vec<String>>,
    /// fields → columns, for entity and ledger tables
    pub fields: Vec<FieldMap>,
}

#[derive(Clone, Debug, Default)]
pub struct Schema {
    pub tables: Vec<Table>,
    /// the indexes the model needs (not part of the fingerprint: they are kept in step by
    /// `Database::ensure_indexes`, see `Schema::indexes`)
    pub indexes: Vec<Index>,
    /// the model may mark values provisional (`provisional(…)`): the markers are in the history
    /// only, so records are not read from their projection rows
    pub markers: bool,
    /// the entities whose records read back from their projection rows exactly as the history
    /// has them: each stored field has columns that say how (`Decode`), a set of references
    /// its join table (`sets`), a per-company field its table
    pub readable: BTreeSet<String>,
    /// entity → its stored sets (and lists) of references, each in a join table
    pub sets: BTreeMap<String, Vec<SetMap>>,
}

/// A stored set (or list) of references to records of one entity, held in a join table (one
/// row per reference, with its position `ord`): the value reads back from it when each
/// reference is there once, `db::fits_refs`.
#[derive(Clone, Debug)]
pub struct SetMap {
    pub field: String,
    pub table: String,
    /// the entity of the references
    pub target: String,
    /// a set (`{T}`), or a list (`[T]`)
    pub set: bool,
}

pub fn snake(name: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            let prev_lower = i > 0 && chars[i - 1].is_lowercase();
            let next_lower = chars.get(i + 1).map_or(false, |n| n.is_lowercase());
            if i > 0 && (prev_lower || (next_lower && chars[i - 1].is_uppercase())) {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(*c);
        }
    }
    out
}

fn col(name: &str, ty: ColType, part: Part) -> Column {
    Column { name: name.to_string(), ty, part, not_null: false, check: None, references: None }
}

/// Is this collection field computed from the other side (children, inverse relation)?
pub fn is_inverse(model: &Model, owner: &quadra_ir::Entity, f: &Field) -> bool {
    let Some(Ty::List(inner) | Ty::Set(inner)) = &f.ty else {
        return false;
    };
    let Ty::Path { segments, .. } = inner.as_ref() else {
        return false;
    };
    let Some(child) = model.entity(&segments[0]) else {
        return false;
    };
    if child.kind == EntityKind::Record {
        return false;
    }
    if f.through.is_some() || f.via.is_some() {
        return true;
    }
    child.fields.iter().any(|x| {
        let n = match &x.ty {
            Some(Ty::Path { segments, .. }) => Some(segments[0].clone()),
            Some(Ty::Optional(i)) => match i.as_ref() {
                Ty::Path { segments, .. } => Some(segments[0].clone()),
                _ => None,
            },
            _ => None,
        };
        n.map_or(false, |n| n == owner.name || owner.traits.contains(&n))
    })
}

pub struct Builder<'m> {
    model: &'m Model,
    pub traits_used: BTreeSet<String>,
}

impl<'m> Builder<'m> {
    fn target_table(&mut self, ent: &str) -> Option<String> {
        let e = self.model.entity(ent)?;
        match e.kind {
            EntityKind::Record => None,
            EntityKind::Trait => {
                self.traits_used.insert(ent.to_string());
                Some(snake(ent))
            }
            EntityKind::Entity => Some(snake(ent)),
        }
    }

    /// Columns for a value of type `t` stored under `name`.
    fn columns(&mut self, name: &str, t: &T, enum_ty: Option<&Ty>) -> Option<Vec<Column>> {
        Some(match t {
            T::Opt(i) => return self.columns(name, i, enum_ty),
            T::Bool => vec![col(name, ColType::Bool, Part::Scalar)],
            T::Num(Num::Int) => vec![col(name, ColType::BigInt, Part::Scalar)],
            T::Num(_) | T::Zero => vec![col(name, ColType::Numeric, Part::Scalar)],
            T::Text(_) | T::YearMonth | T::Role | T::Variant(_) => {
                vec![col(name, ColType::Text, Part::Scalar)]
            }
            T::Date => vec![col(name, ColType::Date, Part::Scalar)],
            T::DateTime => vec![col(name, ColType::Timestamp, Part::Scalar)],
            T::Duration => vec![col(name, ColType::BigInt, Part::Duration)],
            T::DateRange => vec![col(name, ColType::Json, Part::Json)],
            T::Money { .. } => vec![col(name, ColType::Numeric, Part::Amount), col(&format!("{name}__cur"), ColType::Text, Part::Currency)],
            T::Price { .. } => vec![col(name, ColType::Numeric, Part::Amount), col(&format!("{name}__cur"), ColType::Text, Part::Currency), col(&format!("{name}__per"), ColType::Text, Part::Per)],
            T::Qty { .. } => vec![col(name, ColType::Numeric, Part::Amount), col(&format!("{name}__unit"), ColType::Text, Part::Unit)],
            T::Ent(e) => match self.target_table(e) {
                Some(table) => {
                    let mut c = col(&format!("{name}_id"), ColType::BigInt, Part::RefId);
                    c.references = Some(table);
                    vec![c]
                }
                None => vec![col(name, ColType::Json, Part::Json)],
            },
            T::Enum { variants, .. } => {
                let payload = enum_ty
                    .and_then(inline_enum)
                    .map(|vs| vs.iter().any(|v| !v.fields.is_empty()))
                    .or_else(|| self.model.enums.iter().find(|e| e.variants.iter().any(|v| variants.contains(&v.name))).map(|e| e.variants.iter().any(|v| !v.fields.is_empty())))
                    .unwrap_or(false);
                if payload {
                    vec![col(name, ColType::Json, Part::Json)]
                } else {
                    let mut c = col(name, ColType::Text, Part::Scalar);
                    let list: Vec<String> = variants.iter().map(|v| format!("'{v}'")).collect();
                    c.check = Some(format!("\"{name}\" IN ({})", list.join(", ")));
                    vec![c]
                }
            }
            T::Coll { .. } | T::Tuple(_) => vec![col(name, ColType::Json, Part::Json)],
            _ => return None,
        })
    }

    /// How the columns `columns` makes for a value of type `t` read back.
    fn decode(&mut self, t: &T, enum_ty: Option<&Ty>) -> Decode {
        match t {
            T::Opt(i) => self.decode(i, enum_ty),
            T::Bool => Decode::Bool,
            T::Num(Num::Int) => Decode::Int,
            T::Num(Num::Decimal) => Decode::Dec,
            T::Num(Num::Percent) => Decode::Pct,
            T::Text(_) => Decode::Text,
            T::YearMonth => Decode::YearMonth,
            T::Role => Decode::Role,
            T::Variant(_) => Decode::Variant,
            T::Date => Decode::Date,
            T::DateTime => Decode::DateTime,
            T::Duration => Decode::Duration,
            T::DateRange | T::Coll { .. } | T::Tuple(_) => Decode::Json,
            T::Money { .. } => Decode::Money,
            T::Price { .. } => Decode::Price,
            T::Qty { .. } => Decode::Qty,
            T::Ent(e) => match self.model.entity(e).map(|x| x.kind) {
                Some(EntityKind::Entity) => Decode::Ref(e.clone()),
                // a value record: its JSON; a trait: the entity of the record is not in the row
                Some(EntityKind::Record) => Decode::Json,
                _ => Decode::Unknown,
            },
            T::Enum { .. } => match self.columns("x", t, enum_ty).as_ref().and_then(|c| c.first()).map(|c| c.part) {
                Some(Part::Json) => Decode::Json,
                Some(Part::Scalar) => Decode::Variant,
                _ => Decode::Unknown,
            },
            _ => Decode::Unknown,
        }
    }
}

pub fn build(model: &Model, report: &Report) -> Schema {
    let mut b = Builder { model, traits_used: BTreeSet::new() };
    let mut tables = Vec::new();
    let mut readable = BTreeSet::new();
    let mut sets: BTreeMap<String, Vec<SetMap>> = BTreeMap::new();
    for e in &model.entities {
        // temporary entities live only inside a migration
        if e.kind != EntityKind::Entity || e.temporary {
            continue;
        }
        let table = snake(&e.name);
        let mut columns = vec![col("id", ColType::BigInt, Part::Scalar)];
        if e.is_ledger {
            columns.extend([
                col("_tx", ColType::BigInt, Part::Scalar),
                col("_date", ColType::Timestamp, Part::Scalar),
                col("_source_entity", ColType::Text, Part::Scalar),
                col("_source_id", ColType::BigInt, Part::Scalar),
            ]);
        } else {
            columns.extend([
                col("_created_tx", ColType::BigInt, Part::Scalar),
                col("_created_at", ColType::Timestamp, Part::Scalar),
                col("_created_by", ColType::BigInt, Part::Scalar),
                col("_frozen_tx", ColType::BigInt, Part::Scalar),
            ]);
        }
        let mut fields = Vec::new();
        let mut uniques: Vec<Vec<String>> = Vec::new();
        let mut complete = !e.is_ledger;
        for f in model.all_fields(e) {
            let formula = f.formula.is_some();
            if is_inverse(model, e, f) {
                continue;
            }
            let t = report.field_types.get(&(e.name.clone(), f.name.clone())).cloned().or_else(|| {
                // trait fields: typed in the trait that declares them (a trait of a trait too)
                model.all_traits(e).iter().find_map(|tr| report.field_types.get(&(tr.name.clone(), f.name.clone())).cloned())
            });
            let Some(t) = t else {
                complete &= formula;
                continue;
            };
            if f.per_company {
                if !formula {
                    // its own table: (owner, company) → value
                    let decode = b.decode(&t, f.ty.as_ref());
                    complete &= decode != Decode::Unknown;
                    if let Some(cols) = b.columns("value", &t, f.ty.as_ref()) {
                        let mut owner = col("owner_id", ColType::BigInt, Part::Scalar);
                        owner.references = Some(table.clone());
                        owner.not_null = true;
                        let mut company = col("company_id", ColType::BigInt, Part::Scalar);
                        company.references = b.target_table("Company");
                        company.not_null = true;
                        let mut all = vec![owner, company];
                        all.extend(cols.iter().cloned());
                        tables.push(Table {
                            name: format!("{table}__{}", f.name),
                            source: Source::PerCompany(e.name.clone(), f.name.clone()),
                            columns: all,
                            primary: vec!["owner_id".into(), "company_id".into()],
                            uniques: Vec::new(),
                            fields: vec![FieldMap { field: f.name.clone(), formula: false, columns: cols, decode, incremental: false }],
                        });
                    } else {
                        complete = false;
                    }
                }
                continue;
            }
            if !formula && matches!(t.unopt(), T::Coll { elem, .. } if matches!(elem.as_ref(), T::Ent(_))) {
                // a collection of value records (JSON): read from the history
                let value_records = matches!(t.unopt(), T::Coll { elem, .. } if matches!(elem.as_ref(), T::Ent(x) if model.entity(x).is_some_and(|x| x.kind == EntityKind::Record)));
                complete &= !value_records;
            }
            // stored set of references: a join table
            if let (false, T::Coll { elem, set, .. }) = (formula, t.unopt()) {
                if let T::Ent(target) = elem.as_ref() {
                    if let Some(tt) = b.target_table(target) {
                        // references to a trait: the entity of each is not in the join table
                        if model.entity(target).is_some_and(|x| x.kind == EntityKind::Entity) {
                            sets.entry(e.name.clone()).or_default().push(SetMap { field: f.name.clone(), table: format!("{table}__{}", f.name), target: target.clone(), set: *set });
                        } else {
                            complete = false;
                        }
                        let mut owner = col("owner_id", ColType::BigInt, Part::Scalar);
                        owner.references = Some(table.clone());
                        owner.not_null = true;
                        let mut tid = col("target_id", ColType::BigInt, Part::RefId);
                        tid.references = Some(tt);
                        tid.not_null = true;
                        // the position of the reference in the set: it reads back in its order
                        let ord = col("ord", ColType::BigInt, Part::Scalar);
                        tables.push(Table {
                            name: format!("{table}__{}", f.name),
                            source: Source::Join(e.name.clone(), f.name.clone()),
                            columns: vec![owner, tid, ord],
                            primary: vec!["owner_id".into(), "target_id".into()],
                            uniques: Vec::new(),
                            fields: Vec::new(),
                        });
                        continue;
                    }
                }
            }
            let Some(mut cols) = b.columns(&f.name, &t, f.ty.as_ref()) else {
                complete &= formula;
                continue;
            };
            if formula {
                // materialized: plain nullable columns, no constraints
                for c in &mut cols {
                    c.check = None;
                    c.references = None;
                }
            } else {
                // an empty collection is stored as NULL
                let optional = matches!(t, T::Coll { .. }) || matches!(f.ty, Some(Ty::Optional(_))) || f.required_when.is_some() || f.kind != FieldKind::Stored && !e.is_ledger;
                if !optional && !e.is_ledger {
                    if let Some(first) = cols.first_mut() {
                        first.not_null = true;
                    }
                }
                if f.unique && cols.len() == 1 {
                    uniques.push(vec![cols[0].name.clone()]);
                }
            }
            columns.extend(cols.iter().cloned());
            let decode = b.decode(&t, f.ty.as_ref());
            let incremental = formula && quadra_analysis::deps::incremental(model, &e.name, &f.name).is_ok();
            fields.push(FieldMap { field: f.name.clone(), formula, columns: cols, decode, incremental });
        }
        // unconditional uniques on stored columns (own and from traits)
        for u in model.all_uniques(e) {
            if u.when.is_some() {
                continue;
            }
            let cols: Option<Vec<String>> = u.fields.iter().map(|f| fields.iter().find(|m| &m.field == f && !m.formula).and_then(|m| (m.columns.len() == 1).then(|| m.columns[0].name.clone()))).collect();
            if let Some(c) = cols {
                if !uniques.contains(&c) {
                    uniques.push(c);
                }
            }
        }
        if complete && fields.iter().all(|fm: &FieldMap| fm.formula || fm.decode != Decode::Unknown) {
            readable.insert(e.name.clone());
        }
        tables.push(Table { name: table, source: if e.is_ledger { Source::Ledger(e.name.clone()) } else { Source::Entity(e.name.clone()) }, columns, primary: vec!["id".into()], uniques, fields });
    }
    for t in b.traits_used.clone() {
        let mut ent = col("entity", ColType::Text, Part::Scalar);
        ent.not_null = true;
        tables.push(Table { name: snake(&t), source: Source::Trait(t), columns: vec![col("id", ColType::BigInt, Part::Scalar), ent], primary: vec!["id".into()], uniques: Vec::new(), fields: Vec::new() });
    }
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    let markers = serde_json::to_string(model).map_or(true, |m| m.contains("\"provisional\""));
    if markers {
        readable.clear();
    }
    let mut schema = Schema { tables, indexes: Vec::new(), markers, readable, sets };
    schema.indexes = schema.indexes(model);
    schema
}

impl Column {
    /// The column's definition in `CREATE TABLE` / `ADD COLUMN` (NOT NULL when `not_null`).
    pub fn definition(&self, d: Dialect, not_null: bool) -> String {
        let mut s = format!("\"{}\" {}", self.name, self.ty.sql(d));
        if not_null {
            s.push_str(" NOT NULL");
        }
        if let Some(ch) = &self.check {
            s.push_str(&format!(" CHECK ({ch})"));
        }
        s
    }
}

impl Table {
    /// `CREATE TABLE` (on SQLite with its foreign keys; on PostgreSQL they are added after
    /// every table exists: `foreign_keys`).
    pub fn create_sql(&self, d: Dialect) -> String {
        self.create_sql_as(&self.name, d)
    }

    /// `CREATE TABLE` under another name (a table rebuilt next to the old one).
    pub fn create_sql_as(&self, name: &str, d: Dialect) -> String {
        // SQLite: an `id` key that is the table's rowid (`INTEGER PRIMARY KEY`): one B-tree
        // instead of the table's and the key's index
        let rowid = d == Dialect::Sqlite && self.primary.len() == 1 && self.column(&self.primary[0]).is_some_and(|c| c.ty == ColType::BigInt);
        let mut parts: Vec<String> =
            self.columns.iter().map(|c| if rowid && self.primary[0] == c.name { format!("\"{}\" INTEGER PRIMARY KEY", c.name) } else { c.definition(d, c.not_null || self.primary.contains(&c.name)) }).collect();
        if !rowid {
            parts.push(format!("PRIMARY KEY ({})", self.primary.iter().map(|p| format!("\"{p}\"")).collect::<Vec<_>>().join(", ")));
        }
        if d == Dialect::Sqlite {
            for c in &self.columns {
                if let Some(r) = &c.references {
                    parts.push(format!("FOREIGN KEY (\"{}\") REFERENCES \"{r}\" (\"id\") DEFERRABLE INITIALLY DEFERRED", c.name));
                }
            }
        }
        for u in &self.uniques {
            parts.push(format!("UNIQUE ({})", u.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ")));
        }
        format!("CREATE TABLE \"{name}\" (\n  {}\n)", parts.join(",\n  "))
    }

    /// PostgreSQL: the foreign keys of the table, as `ALTER TABLE` statements.
    pub fn foreign_keys(&self) -> Vec<String> {
        self.columns.iter().filter_map(|c| c.references.as_ref().map(|r| foreign_key(&self.name, &c.name, r))).collect()
    }

    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }
}

/// PostgreSQL: a foreign key added to a table.
pub fn foreign_key(table: &str, col: &str, target: &str) -> String {
    format!("ALTER TABLE \"{table}\" ADD FOREIGN KEY (\"{col}\") REFERENCES \"{target}\" (\"id\") DEFERRABLE INITIALLY DEFERRED")
}

/// An index of a projection table, derived from the model (see `indexes`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    /// `qx_<table>_<columns>` (shortened with a hash past 63 characters)
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    /// why the model needs it: `reference Order.customer`, `set Region.team`, …
    pub why: String,
    /// on the ASCII lower case of the (text) column: the search of `GET /v1/search`
    #[serde(default)]
    pub lower: bool,
    /// the full-text search of a text column: a GIN index on its words on PostgreSQL; on
    /// SQLite the columns of a table are in one FTS5 table (`fts_table`, kept by triggers,
    /// `Database::sync_search`)
    #[serde(default)]
    pub fts: bool,
}

/// SQLite: the FTS5 table of the searched columns of a table (its columns are `c0`, `c1`…, in
/// the order of `Schema::search_columns`).
pub fn fts_table(table: &str) -> String {
    format!("qx_{table}_fts")
}

/// PostgreSQL: the words of a text column as the search indexes them (`query::words`: runs of
/// letters and digits, in lower case), as a `tsvector`. The index and the queries use the same
/// expression.
pub fn fts_vector_pg(col: &str) -> String {
    format!("to_tsvector('simple'::regconfig, regexp_replace(coalesce({col}, ''), '[^[:alnum:]]+', ' ', 'g'))")
}

impl Index {
    pub fn create_sql(&self, d: Dialect, concurrently: bool) -> String {
        if self.fts {
            // (SQLite: the FTS5 table, `Database::sync_search`)
            let c = format!("\"{}\"", self.columns[0]);
            return format!("CREATE INDEX {}IF NOT EXISTS \"{}\" ON \"{}\" USING gin ({})", if concurrently { "CONCURRENTLY " } else { "" }, self.name, self.table, fts_vector_pg(&c));
        }
        let cols: Vec<String> = self
            .columns
            .iter()
            .map(|c| match (self.lower, d) {
                (false, _) => format!("\"{c}\""),
                (true, Dialect::Sqlite) => format!("lower(\"{c}\")"),
                // ASCII lower case, compared byte by byte: what the search compares
                (true, Dialect::Postgres) => format!("(lower(\"{c}\" COLLATE \"C\"))"),
            })
            .collect();
        format!("CREATE INDEX {}IF NOT EXISTS \"{}\" ON \"{}\" ({})", if concurrently { "CONCURRENTLY " } else { "" }, self.name, self.table, cols.join(", "))
    }
}

/// The fields a record is shown by, in order (`Interp::display`), and searched by.
pub const DISPLAY_FIELDS: &[&str] = &["number", "code", "name", "login"];

/// The stable name of an index: `qx_<table>_<columns>`, at most 63 characters (PostgreSQL).
pub fn index_name(table: &str, cols: &[String]) -> String {
    let n = format!("qx_{table}_{}", cols.join("_"));
    if n.len() <= 63 {
        return n;
    }
    let h: String = Sha256::digest(n.as_bytes()).iter().take(4).map(|b| format!("{b:02x}")).collect();
    let mut cut = 54;
    while !n.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}_{h}", &n[..cut])
}

impl Schema {
    pub fn ddl(&self, d: Dialect) -> Vec<String> {
        let mut out: Vec<String> = self.tables.iter().map(|t| t.create_sql(d)).collect();
        // PostgreSQL checks that the referenced table exists: foreign keys come after every table
        if d == Dialect::Postgres {
            for t in &self.tables {
                out.extend(t.foreign_keys());
            }
        }
        out
    }

    /// The indexes the model needs on the projection tables (the access paths of the engine
    /// and of SQL over the projections):
    /// - every reference column of an entity or ledger table (`parent` fields included): the
    ///   inverse relations (`customer.orders`) and the rules that follow references;
    /// - the target side of every many-to-many join table (the owner side is its primary key):
    ///   `Tag.customers via tags`, `region.team.contains(me)`;
    /// - the columns of `unique` declarations that are not a constraint (`unique … when`);
    /// - the single-column fields compared with `==` in row-permission rules.
    /// A column already first in a `UNIQUE` constraint or the primary key is not indexed again.
    fn indexes(&self, model: &Model) -> Vec<Index> {
        let mut out: Vec<Index> = Vec::new();
        let add = |out: &mut Vec<Index>, t: &Table, cols: Vec<String>, why: String| {
            let covered = t.primary.first() == cols.first() && t.primary.len() >= cols.len() && t.primary[..cols.len()] == cols[..] || t.uniques.iter().any(|u| u.len() >= cols.len() && u[..cols.len()] == cols[..]);
            let name = index_name(&t.name, &cols);
            if !covered && !out.iter().any(|i| i.name == name) {
                out.push(Index { name, table: t.name.clone(), columns: cols, why, lower: false, fts: false });
            }
        };
        for t in &self.tables {
            match &t.source {
                Source::Entity(e) | Source::Ledger(e) => {
                    for fm in t.fields.iter().filter(|f| !f.formula) {
                        for c in fm.columns.iter().filter(|c| c.part == Part::RefId) {
                            add(&mut out, t, vec![c.name.clone()], format!("reference {e}.{}", fm.field));
                        }
                    }
                    if let (Source::Entity(e), Some(ent)) = (&t.source, model.entity(e)) {
                        for u in model.all_uniques(ent).iter().filter(|u| u.when.is_some()) {
                            let cols: Option<Vec<String>> = u.fields.iter().map(|f| t.fields.iter().find(|m| &m.field == f && !m.formula && m.columns.len() == 1).map(|m| m.columns[0].name.clone())).collect();
                            if let Some(cols) = cols {
                                add(&mut out, t, cols, format!("unique ({}) when … of {e}", u.fields.join(", ")));
                            }
                        }
                    }
                }
                Source::Join(e, f) => add(&mut out, t, vec!["target_id".into()], format!("set {e}.{f}")),
                _ => {}
            }
        }
        // fields compared with `==` in row rules (`c.salesperson == me`, `o.state == …`)
        for rule in model.roles.iter().flat_map(|r| r.rules.iter()).chain(model.rules.iter()) {
            let (Some(target), Some(cond)) = (rule.targets.first().and_then(|t| t.first()), &rule.condition) else {
                continue;
            };
            let ExprKind::Lambda { params, body } = &cond.kind else {
                continue;
            };
            let Some(p) = params.first() else { continue };
            let mut found = Vec::new();
            compared_fields(model, body, p, target, &mut found);
            for (ent, field) in found {
                let Some(t) = self.tables.iter().find(|t| matches!(&t.source, Source::Entity(e) if *e == ent)) else {
                    continue;
                };
                let Some(fm) = t.fields.iter().find(|m| m.field == field && !m.formula && m.columns.len() == 1 && m.columns[0].part != Part::Json) else {
                    continue;
                };
                add(&mut out, t, vec![fm.columns[0].name.clone()], format!("row rule on {ent}.{field}"));
            }
        }
        for t in &self.tables {
            let Source::Entity(e) = &t.source else {
                continue;
            };
            for fm in t.fields.iter().filter(|f| !f.formula && f.columns.len() == 1 && f.columns[0].part == Part::Scalar) {
                let c = &fm.columns[0];
                // dates: documents are listed by date (an index orders a page without sorting)
                if matches!(c.ty, ColType::Date | ColType::Timestamp) {
                    // with the id: the order of a page (ties by id) is the index's
                    add(&mut out, t, vec![c.name.clone(), "id".into()], format!("order by {e}.{}", fm.field));
                }
                // what a record is shown by: the search, prefix first (ASCII case-insensitive)
                if c.ty == ColType::Text && c.check.is_none() && DISPLAY_FIELDS.contains(&fm.field.as_str()) {
                    let name = index_name(&t.name, &[format!("lower_{}", c.name)]);
                    if !out.iter().any(|i| i.name == name) {
                        out.push(Index { name, table: t.name.clone(), columns: vec![c.name.clone()], why: format!("search {e}.{}", fm.field), lower: true, fts: false });
                    }
                }
            }
            // the full-text search: the stored text fields (`query::searchable`)
            for f in quadra_runtime::query::searchable(model, e) {
                let Some(fm) = t.fields.iter().find(|m| m.field == f && !m.formula && m.columns.len() == 1) else {
                    continue;
                };
                let c = &fm.columns[0];
                if c.ty != ColType::Text || c.check.is_some() || c.part != Part::Scalar {
                    continue;
                }
                let name = index_name(&t.name, &["fts".to_string(), c.name.clone()]);
                if !out.iter().any(|i| i.name == name) {
                    out.push(Index { name, table: t.name.clone(), columns: vec![c.name.clone()], why: format!("full-text search {e}.{f}"), lower: false, fts: true });
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// The columns of a table in its full-text search index, in their order.
    pub fn search_columns(&self, table: &str) -> Vec<String> {
        let mut cols: Vec<(String, String)> = self.indexes.iter().filter(|i| i.fts && i.table == table).map(|i| (i.name.clone(), i.columns[0].clone())).collect();
        cols.sort();
        cols.into_iter().map(|(_, c)| c).collect()
    }

    /// The indexes of one table (the catalog per entity: `Schema::entity_table`).
    pub fn indexes_of(&self, table: &str) -> Vec<&Index> {
        self.indexes.iter().filter(|i| i.table == table).collect()
    }

    /// The table of an entity or ledger.
    /// The per-company table of a field of an entity.
    pub fn per_company_table(&self, ent: &str, field: &str) -> Option<&Table> {
        self.tables.iter().find(|t| matches!(&t.source, Source::PerCompany(e, f) if e == ent && f == field))
    }

    /// The per-company tables of an entity.
    pub fn per_company_tables(&self, ent: &str) -> Vec<&Table> {
        self.tables.iter().filter(|t| matches!(&t.source, Source::PerCompany(e, _) if e == ent)).collect()
    }

    pub fn entity_table(&self, ent: &str) -> Option<&Table> {
        self.tables.iter().find(|t| matches!(&t.source, Source::Entity(e) | Source::Ledger(e) if e == ent))
    }

    /// The single column holding a stored field of an entity, when an index has it first:
    /// a lookup by value is an index search (`DbSource::find`, and SQL built over the
    /// projections).
    pub fn indexed_column(&self, ent: &str, field: &str) -> Option<(&Table, &Column)> {
        let t = self.entity_table(ent)?;
        let fm = t.fields.iter().find(|f| f.field == field && !f.formula && f.columns.len() == 1)?;
        let c = &fm.columns[0];
        let indexed =
            self.indexes.iter().any(|i| i.table == t.name && !i.lower && !i.fts && i.columns.first() == Some(&c.name)) || t.uniques.iter().any(|u| u.first() == Some(&c.name)) || t.primary.first() == Some(&c.name);
        indexed.then_some((t, c))
    }

    /// Stable fingerprint of the schema (independent of the dialect).
    pub fn fingerprint(&self) -> String {
        let mut h = Sha256::new();
        for s in self.ddl(Dialect::Postgres) {
            h.update(s.as_bytes());
        }
        let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        hex[..16].to_string()
    }

    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name == name)
    }
}

/// The (entity, field) at the end of every `param.a.b == …` of a rule's condition.
fn compared_fields(model: &Model, e: &Expr, param: &str, root: &str, out: &mut Vec<(String, String)>) {
    // the entity and field a path from the parameter ends on
    fn resolve(model: &Model, e: &Expr, param: &str, root: &str) -> Option<(String, String)> {
        let mut names = Vec::new();
        let mut cur = e;
        loop {
            match &cur.kind {
                ExprKind::Field { base, name, .. } => {
                    names.push(name.clone());
                    cur = base;
                }
                ExprKind::Name(n) if n == param => break,
                _ => return None,
            }
        }
        names.reverse();
        let mut ent = root.to_string();
        for (i, n) in names.iter().enumerate() {
            let en = model.entity(&ent)?;
            let f = model.find_field(en, n)?;
            if i + 1 == names.len() {
                return Some((ent, n.clone()));
            }
            ent = match f.ty.as_ref()? {
                Ty::Path { segments, .. } => segments.first()?.clone(),
                Ty::Optional(inner) => match inner.as_ref() {
                    Ty::Path { segments, .. } => segments.first()?.clone(),
                    _ => return None,
                },
                _ => return None,
            };
        }
        None
    }
    match &e.kind {
        ExprKind::Binary { op: BinOp::Eq, lhs, rhs } => {
            for side in [lhs, rhs] {
                if let Some(x) = resolve(model, side, param, root) {
                    if !out.contains(&x) {
                        out.push(x);
                    }
                }
            }
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            compared_fields(model, lhs, param, root, out);
            compared_fields(model, rhs, param, root, out);
        }
        ExprKind::Unary { expr, .. } => compared_fields(model, expr, param, root, out),
        ExprKind::If { cond, then, otherwise } => {
            for x in [cond, then, otherwise] {
                compared_fields(model, x, param, root, out);
            }
        }
        _ => {}
    }
}
