# Storage

Quadra stores data in SQLite or PostgreSQL, interchangeably. The truth is an append-only
history of every value, chained by hashes into a signed commit log; readable SQL tables, one
per entity, are kept up to date next to it in the same transactions. The engine loads what
it needs on demand, through a shared cache, and commits with optimistic concurrency.

## Database URLs

| URL | Database |
|---|---|
| `sqlite:data.db`, `sqlite:/abs/path/data.db` | a SQLite file (WAL mode, foreign keys on) |
| `sqlite::memory:` | a SQLite database in memory |
| `postgres://user@host:5432/db` (or `postgresql://`) | PostgreSQL, without TLS |
| `postgres://user@host:5432/db#schema=tenant_a` | a PostgreSQL schema, created if needed: one database can hold several projects or tenants |

Commands take `--url`; otherwise `[database] url` in `quadra.toml` (a relative SQLite path is
resolved from the project root); otherwise `sqlite:<root>/.quadra/data.db`. Any other URL is
refused: ``unsupported database url `x` (use sqlite:… or postgres://…)``.

The same tests produce byte-for-byte the same history, and the same hash chain, on both
databases.

## What is stored

### The truth

| Table | Content |
|---|---|
| `_quadra_commits` | one row per transaction: `tx`, `at`, `user_id`, `cause`, `prev_hash`, `hash` |
| `_quadra_history` | every value written to every field, with its transaction, in order |
| `_quadra_records` | every record: entity, creation (transaction, time, user), `frozen_tx`, cause, warnings; `last_tx`, the last transaction that changed its row in any way (a field, its metadata, a computed value), and `version_tx`, the last one that wrote its fields when its projection row reads back exactly (0: the record is read from its history) |
| `_quadra_ledger_rows` | every ledger row: ledger, transaction, date, source, columns |
| `_quadra_postings` | what each record currently posts to each ledger |
| `_quadra_counters` | the counters of `next_number` |
| `_quadra_renames` | the field renames of migrations, with the transaction from which they apply |
| `_quadra_applied` | the migrations and changes applied, and the reverts |
| `_quadra_tokens`, `_quadra_idempotency` | API credentials (hashed) and idempotency keys |
| `_quadra_outbox` | the events emitted by `emit`, written with their commit, and whether they were delivered ([The outbox](/reference/flows-and-actions#the-outbox)); not part of the commit hash |
| `_quadra_meta` | the schema fingerprint, the catalog of the deployed model (its tables, rules, formulas and indexes: what the next deploy compares with) and the engine's counters |

Nothing in the history is ever updated or deleted. A correction is a new write; a rename is
a mapping applied when reading, so the signatures of past commits stay valid; a reverted
change is a new transaction.

### The signed commit log

Every transaction of the engine is one database transaction, which writes its history rows,
ledger rows and a commit row with

```text
hash = sha256(prev_hash | tx | at | user | cause | the values and ledger rows written)
```

Each commit includes the hash of the previous one, so altering, removing or reordering
anything in the past breaks the chain. `quadra db verify` recomputes the chain and reports,
for example, `commit 12: content does not match its hash (history or ledger rows were altered)`.

### The tables

Next to the truth, the database holds tables that anyone can read with SQL. They are written
in the same transaction as the history, and changed by deploys only where the model changed
([What a deploy changes](/reference/migrations-and-changes#what-a-deploy-changes-in-the-database)).

| Model | Table |
|---|---|
| an entity `SalesOrder` | `sales_order` (snake case) |
| a ledger `Stock` | `stock`, with `_tx`, `_date`, `_source_entity`, `_source_id` |
| a trait used as a type | `document`: `(id, entity)` of every implementor, so that a reference to a trait is a real foreign key |
| a stored set `tags: {Tag}` | a join table `partner__tags (owner_id, target_id, ord)`, `ord` the position of the reference in the set |
| a `per company` field | `product__list_price (owner_id, company_id, …)` |

| Field | Columns |
|---|---|
| system | `id`, `_created_tx`, `_created_at`, `_created_by`, `_frozen_tx` |
| `Text`, `Code`… | `TEXT` |
| `Money` | `<field>` (the amount) + `<field>__cur` |
| `Price` | `<field>` + `<field>__cur` + `<field>__per` |
| `Qty` | `<field>` + `<field>__unit` |
| a reference | `<field>_id`, with a foreign key |
| an enum | `TEXT` with a `CHECK` on the variants |
| `unique` | a `UNIQUE` constraint |
| a formula | a column with its current value (materialized), kept up to date by every commit |

Collections computed from the other side (`lines: [OrderLine]`) have no column.
Decimals keep their scale (`7.50` stays `7.50`), so a row reads back exactly as the history
has its values.
Foreign keys are `DEFERRABLE INITIALLY DEFERRED`, because one transaction can create records
that refer to each other.

| Type | SQLite | PostgreSQL |
|---|---|---|
| decimals, amounts | `TEXT` | `NUMERIC` |
| dates, date-times | `TEXT` | `DATE`, `TIMESTAMP` |
| booleans | `INTEGER` | `BOOLEAN` |
| structured values | `TEXT` | `JSONB` |

`quadra db verify` compares every column of every table, formulas included, with the values
rebuilt from the history, and reports tables changed by hand.

### Indexes

Every access path of the engine is an index search, never a scan of the history:

| Table | Indexes |
|---|---|
| `_quadra_history` | `(record_id, seq)`: a record's history; `(tx)`: the writes of a transaction |
| `_quadra_records` | `(entity, id)`: the records of an entity; `(last_tx)`, `(created_tx)`: what changed after a transaction |
| `_quadra_ledger_rows` | `(ledger, tx)` |
| the tables of the model | every reference column; the far side of every many-to-many join table; the fields of a `unique … when`; the fields compared with `==` in row permissions; every stored `Date` and `DateTime` field (pages ordered by date); the ASCII lower case of the `number`, `code`, `name` and `login` text fields (the search, `qx_<table>_lower_<column>`) |

The indexes of the model are named `qx_<table>_<columns>`; they follow the model at every
deploy (on PostgreSQL, those of existing tables are built `CONCURRENTLY` after the commit). A
database created by an older version gets the indexes of the system tables when it is opened,
and those of the model at its next deploy, or when `quadra serve` starts. Indexes are inferred,
not declared: the editor's hints say when a query reads every row of a table for lack of one.

### Full-text search

The stored text fields of each entity (`Text`, `Code`, `Email`, `Phone`, `VatNumber`,
`Barcode`; not formulas, not `per company`) have a search index of their **words** — runs of
letters and digits, in lower case, accents kept — searched by word prefix (`search` of
`POST /v1/query`, `GET /v1/search`):

| Database | Index |
|---|---|
| PostgreSQL | a GIN index per field, `qx_<table>_fts_<column>`, on `to_tsvector('simple', regexp_replace(column, '[^[:alnum:]]+', ' ', 'g'))`; queries use the same expression (`@@ to_tsquery('simple', 'word:*')`). `pg_trgm` is not needed |
| SQLite | one FTS5 table per entity, `qx_<table>_fts` (no copy of the text: `contentless_delete`), its columns the fields' words (`quadra_words(column)`, a function of every connection), by record id; three triggers on the table keep it up to date on every insert, update and delete |

Every commit keeps it up to date. A bulk import larger than the database fills it once per
table at the end; a deploy that adds, renames, removes or retypes a text field rebuilds the
entity's index (`_quadra_meta` key `search:<table>` holds its fields on SQLite; on PostgreSQL
the GIN indexes follow the model like the others). A field hidden from a user is left out of
that user's searches.

## Loading on demand

The engine does not load the whole database. A request works on a **working set** read at a
transaction, its `base` (the latest by default):

- a record is loaded when it is read, from its projection row (one indexed read), with the
  values of its formulas; the records of a page, and the records its cells read, are loaded
  in a few batches. A record is read from its history instead when its row changed after the
  `base` (`last_tx`), when the row cannot hold its values exactly (a set of references given
  twice, or of a trait; a provisional marker in the model; a record of a database of an older
  version until it is written again), and when a read as of an earlier transaction needs its
  past values. Sets come from their join tables in their order, `per company` values from
  their tables, and a number of another kind than its column says (an integer in a `Decimal`
  field) as the kind it is (`_quadra_records.kinds`). The record and its row are one query
  when the entity it is of is known (a reference, a page of a query);
- queries over a whole entity — `Entity.where(…)` with `count`, `any`, `none`, `all`, `first`,
  `last`, `find`, `sum`, `min`, `max`, `last_by`, `sort_by(…).first()`, and `for x in
  Entity.where(…)` — are answered in SQL: the condition is compiled from the checked
  expression to a parameterized query over the tables of the model (see below), only the
  matching records are loaded (a `count` or a `sum` loads none), a `for` loop reads them page
  by page;
- the ids of an entity are loaded when the code reads it whole (`Product.all()`);
- ledger rows are loaded by key (`Stock.where(s => s.product == p)`).

The SQL answers exactly as the engine would: the translation follows the language's
semantics (none, enums, sets, amounts of one currency, quantities of one unit, byte order of
text), what it cannot translate (a formula, a function, a method without a SQL equivalent) is
evaluated by the engine on the rows SQL returns, and the rows where SQL cannot decide the
same way (a comparison that may meet none or another currency) are decided by the engine.
Inside a transaction a query sees the transaction's own writes: the records it wrote are
evaluated by the engine, and a condition reading another entity it wrote is left to the
engine. As of an older `base`, or when the data changed since the transaction's `base`, the
engine answers. The columns of incremental formulas (below) are read like stored fields when
the transaction has written nothing yet: conditions, orders and aggregates on them are SQL
(a row whose formula column is empty, because the formula has no value or failed, is decided
by the engine). During a deploy (migrations), queries are evaluated by the engine.

### Formulas kept up to date

A formula is **incremental** when everything it reads, transitively, is a stored field or a
formula reached by a path from the record (`lines.sum(l => l.amount)`, `customer.region`,
`orders.where(o => o.state is Confirmed).count()`, a rollup of rollups, a recursion over a
tree such as `depth = parent.depth + 1 if parent != none else 0`). Its value is not computed
when it is read: the engine reads it from its column. Every commit keeps those columns right:

- the writes of the transaction are followed back along what each formula reads, to the
  records whose value may have changed (a line's quantity: its order, its customer, its
  region, its product, the product's category; a category moved in the tree: its subtree);
- a `sum` or `count` over children (`where` filters included) is computed by delta: the
  stored value, plus what the changed children contribute now, minus what they contributed
  before the transaction. A customer moved to another region is subtracted from the old
  region and added to the new one. A `min` or `max` takes the changed children into account
  the same way; when the extreme may have left, the extreme of the other children is one
  indexed query (`ORDER BY … LIMIT 1`);
- the sum (or count) of a ledger's rows posted to the record — `GL.where(g => g.account ==
  self and g.company == company).sum(g => g.amount)`, `Stock.where(s => s.product ==
  self).sum(s => s.qty)` — adds the rows the transaction posts to it (ledger rows are never
  changed: a correction is another row). The other conditions and the value may read the
  row's own columns, the record's stored fields (written in the transaction: the formula is
  computed again) and constants;
- any other formula is computed the usual way, reading the stored values of what did not
  change;
- a frozen record keeps the values of its freezing (they are read, not computed again);
  unfreezing it computes its formulas, and what reads them, on the current data.

A formula is not incremental, and is computed whenever it is read, when it reads something
the analysis cannot follow (a whole entity such as `StockMove.where(…)`, a ledger other than
by the shape above — a condition through a reference of the row such as
`s.location.is_internal`, which changes without a new row —, a function without a body, a
metric), something that changes without a write or depends on who reads it (`me`, `current`,
a `per company` field, `updated_at`, `warnings`, a currency or unit by its code), or the
clock. Its column is still written by every commit, as before. The deploy catalog lists which
formulas are incremental.

Checks read the stored values too: `check open_total <= credit_limit` on a customer costs
the delta of its rollups, not a walk through its orders and lines.

Because the history is append-only, reading at a `base` is simply reading the writes with
`tx <= base`: any past state can be read, which is how frozen records see the world as it was
(a record first read from its projection row loads its history when such a read needs it).

A shared cache keeps recently loaded records (`quadra serve --cache-mb`, 256 MB by default;
`--prewarm-days` preloads the recent ones at start). The cache never serves stale data: before
it serves a newer `base`, it drops the records written since it was last synchronized.

Rules are evaluated incrementally: for each formula, check, automatic transition, `becomes`
reaction, posting and `frozen when`, the engine knows statically which fields it reads, and
re-evaluates it only on the records a transaction can have affected. What the analysis cannot
follow is evaluated on every record of the entity: slower, never wrong. Rules that read the
clock are re-evaluated by the periodic sweep (`quadra db sweep`).

## Optimistic concurrency

A transaction records what it read: the records, the entities it scanned, the values it
looked up, the ledger groups, the counters, the stored values of formulas it relied on. At
commit, under a lock of the commit log
(`BEGIN IMMEDIATE` on SQLite, a table lock on PostgreSQL), the database checks whether any
of it was written after the transaction's `base`:

- if not, the transaction is committed under the next transaction number;
- if so, nothing is written, and the transaction fails with the conflicting commits: who,
  when, why and what (`Product#29.code`, `new Product#29`, `Stock rows with product = #63`,
  `Region#3 (its computed values)`).

Two transactions that change the same rollup (two lines of one order, two customers of one
region) are serialized this way: the second one read the stored value the first changed. A
commit is refused only when a value it read changed: changing a line of a draft order does
not conflict with a transaction that relied only on its region's revenue (draft orders do
not count in it).

Independent transactions commit side by side; transactions that touch the same data are
serialized.

The API runs a request on the latest data, not on the client's `base`: a conflict found at
commit (another commit landed while it ran) makes it run again, a few times at most. The
client's `base` only guards the records the request changes: if someone else changed one of
them since, the answer is
[`409` and the list of conflicts](/reference/http-api#concurrency-base-and-409); the client
reads again and retries from the new `base`.

Transaction numbers are assigned by the database at commit. Record ids are reserved in
blocks and never reused, even after a rollback.

## SQLite or PostgreSQL

| | SQLite | PostgreSQL |
|---|---|---|
| Use | development, tests, demos, small installations | production, several processes |
| Writers | one at a time (`BEGIN IMMEDIATE`) | concurrent processes, serialized on the commit log |
| Backups | copy the file, or Litestream | WAL archiving and point-in-time recovery |
| Tests | `quadra test --db sqlite` | `quadra test --db postgres://…` (a schema per test) |

One `quadra serve` process serves one project and one database. Its engine runs on a single
thread that owns the model, the cache and the connection, so its commits are queued without
locks; the database lock protects against other processes.
