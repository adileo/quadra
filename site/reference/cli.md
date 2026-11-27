# CLI

`quadra` is a single command-line tool: compiler, test runner, database tool, server and
language server.

```text
quadra init [path] [--name n] [--db sqlite|postgres|postgres://…] [--yes] [--no-git]
quadra dev [path] [--url URL] [--port N] [--fixture name] [--user login] [--reset] [--web dir]
quadra check [path]
quadra test [path] [--filter text] [--db sqlite|postgres://…]
quadra trace <Entity.field> [path]
quadra certify [name…] [--approved-by who] [path]
quadra fmt [--check] [path]
quadra db init|status|migrate [--dry-run]|verify|provisional|sweep|fixture <name> [path] [--url URL]
quadra db import <dir> [path] [--url URL] [--commit-size 250000]
quadra db views export <file> | import <file> [path] [--url URL]
quadra change list | preview <name> | apply <name> [--approved-by login] | revert <name> [--approved-by login] [path] [--url URL]
quadra serve [path] [--url URL] [--port 8080] [--web dir] [--dev-login] [--secure-cookies]
             [--cache-mb 256] [--prewarm-days N] [--sweep-every secs] [--editors login,…]
             [--job-timeout 120] [--max-rows N] [--max-list N] [--max-queue 1000]
             [--readers 2] [--writers 1]
quadra token create <login> [--name n] [--read-only] [--days 90] | revoke <name> [path] [--url URL]
quadra outbox list [--pending] [--json] | delivered <id…> [path] [--url URL]
quadra bench gen --scale tiny|small|medium|large --out DIR [--seed N]
quadra bench run [path] --url URL --scale … [--json file] [--timeout 60] [--reps 5] [--only a,b] [--skip a,b]
quadra parse <file>
quadra lsp
quadra --version
```

| Convention | Detail |
|---|---|
| `path` | Any path inside the project; the root is the nearest folder upwards with a `quadra.toml`. Default: the current folder. Error: `no quadra.toml found in <path> or its parents`. |
| Flags | Flags may come before or after the path; a flag's value follows it (`--url sqlite:x.db`). |
| Database | `--url`, else `[database] url` in `quadra.toml`, else `sqlite:<root>/.quadra/data.db`. See [Storage](/reference/storage#database-urls). |
| Errors in the project | The commands that need a model (`trace`, `certify`, `db`, `change`, `token`, `outbox`) stop with ``error: the project has errors, run `quadra check` ``. |
| Exit code | `0` on success, `1` on failure, `2` for a wrong command line (the usage is printed). |

## Projects: `quadra init` and `quadra dev`

### `quadra init`

```bash
mkdir shop && cd shop
quadra init
```

Creates a project in the folder (default: the current one): `quadra.toml`, a starter model
`main.qd`, `tests/main.test.qd` with a `demo` fixture and two tests, `.gitignore` (`.quadra/`)
and a `README.md`, then a git repository with a first commit (the browser code editor commits
to it). It asks, with defaults in brackets:

| Question | Default | Flag |
|---|---|---|
| Project name | the folder's name, in lowercase with `-` | `--name n` |
| Database: sqlite or postgres? | `sqlite` (in `.quadra/data.db`) | `--db sqlite`, `--db postgres` (asks the URL), `--db postgres://…` |
| PostgreSQL URL (for postgres) | `postgres://localhost:5432/<name>` | in `--db` |
| Create a git repository? | yes, if git is installed and the folder is not already in a repository | `--no-git` |

`--yes` (or no terminal) takes every default without asking. It refuses a folder that already
has a `quadra.toml` or a `main.qd`.

### `quadra dev`

```bash
quadra dev
```

The project on your machine, for development: the web app and the code editor on
`http://127.0.0.1:8080` (the next free port if it is taken), applying every change you save.

- **At start**: a new database is created and loaded with the `demo` fixture (or `--fixture
  name`); an existing one gets what changed while the server was off (migrations, new
  defaults, checks); a user `dev` (or `--user login`) is created if there is none, with the
  `Admin` role if the model has one. The variables of `quadra.toml` are read from the
  environment and `.env`; with one missing it says so and the events with an `outbound` wait
  in the outbox ([Variables and connections](/reference/connections)). Code that does not compile, or data that does not fit it,
  is printed and `quadra dev` waits for a fix.
- **On every save** (in your editor or in the browser): the project is checked and applied to
  the database in one transaction — new tables and fields, defaults on existing rows, pending
  migrations, every constraint — then every open page reloads its model and data. Code with
  errors, or data that does not fit (a new required field without a default), is reported in
  the terminal and in a banner of the app; the last version that worked keeps running.
- **Signed in without a password**, as the first user; every user may edit the code from the
  browser (`--editors *`).
- `--reset` (SQLite only) deletes the database and starts again from the fixture.

::: warning For your machine only
`quadra dev` listens on `127.0.0.1` only. In production use `quadra serve`: code changes go
through a commit, the dry run on the live data and a deploy.
:::

## Compiling and testing

### `quadra check`

```bash
quadra check examples/extended
```

Runs every check on the project: syntax, names and imports, types (currencies, units,
rounding, optionals, exhaustive `match`), formula cycles, unreachable states, the
[frozen/live warning](/reference/rules#the-frozen-live-warning), and certified metrics against
`metrics.lock`. Diagnostics are printed with the source line; the summary counts the modules
(the three standard library modules included):

```text
✓ 20 modules checked, 0 warnings
✗ 2 errors, 0 warnings in 4 modules
```

Exits with 1 if there is any error; warnings do not fail.

The project's [interface files](/reference/interfaces) (`interfaces/**/*.qdi`) are checked with
the code: a field, an action or an entity they name that the code no longer has is an error at
its line, with the closest name (or the new name a migration's `rename` gave it) as the fix:

```text
error: `Order` has no field `total`: a migration renamed it `amount_total`: did you mean `amount_total`?
   ┌─ interfaces/sales/cockpit.qdi:25:86
   = fix: write `amount_total` (`quadra check --fix` does it in .qdi files)
```

| Flag | |
|---|---|
| `--json` | the diagnostics as JSON: `{ok, errors, warnings, diagnostics: [{path, line, column, end_line, end_column, severity, message, fix: {replace, with}?}]}` |
| `--fix` | applies the suggested names in the `.qdi` files, then checks again |

### `quadra test`

```bash
quadra test
quadra test --filter invoice
quadra test --db sqlite
quadra test --db postgres://me@localhost:5432/quadra_test
```

| Flag | Effect |
|---|---|
| `--filter text` | runs only the tests whose name contains `text` |
| `--db sqlite` | runs each test on a new temporary SQLite database, loading on demand, then reloads and verifies it |
| `--db postgres://…` | the same on PostgreSQL, in a new schema per test |

Checks the project first. Prints `✓` or `✗` per test, the failure with its location, and
`N passed, M failed`. Exits with 1 if a test fails. See [Testing](/reference/testing).

### `quadra trace`

```bash
quadra trace Order.total examples/minimal
```

Everything a value depends on, who writes the stored values underneath, and what reads it:

```text
Order.total : Money<EUR>    formula · main.qd:30
  = lines.sum(l => l.amount)

Constraints
  frozen when state is not Draft

Depends on
  Order.lines  stored
  OrderLine.amount  = ((qty * price) * (1 - discount)).round()
    OrderLine.discount  stored · default 0%
    OrderLine.price  stored · default product.price
    OrderLine.qty  stored

Stored values underneath, and who writes them
  Order.lines
      user, through forms and API
  OrderLine.discount
      user, through forms and API
  OrderLine.price
      user, through forms and API
  OrderLine.qty
      user, through forms and API

Read by
  Customer.open_total
```

The writers listed include actions, reactions, flows (`flow Draft → Confirmed by confirm`),
migrations and changes, with their location. A ledger appears as `Stock (ledger)` with the
entities that post to it. Error for an unknown target: `` unknown entity `Foo` ``.

### `quadra certify`

```bash
quadra certify
quadra certify revenue dso --approved-by "m.bianchi (CFO)"
```

Computes the fingerprints of metrics and controls and writes `metrics.lock`. Without names,
every metric and control; with names, only those (an unknown name is an error and nothing is
written). `--approved-by` records the approver, before or after the names. Prints `certified revenue: sha256:… (14 dependencies)`
for every new or changed entry. See [Metrics](/reference/metrics).

### `quadra fmt`

```bash
quadra fmt
quadra fmt --check
```

Formats every source file of the project. Only whitespace the grammar ignores is touched:
trailing spaces are removed, runs of blank lines are shortened to two, and the file ends with
one newline. Indentation and the content of triple-quoted strings are left alone. With
`--check`, nothing is written: the files that would change are listed
(`would reformat /…/sales.qd`) and the exit code is 1 if there are any.

### `quadra parse`

```bash
quadra parse sales.qd
```

Prints the syntax tree of one file, and its syntax errors. For debugging the parser.

### `quadra lsp`

Starts the language server on standard input and output. Editors start it themselves: the
VS Code and Cursor extension is in `editors/vscode`. It reports the errors of every phase while
you type, and offers go to definition, hover with the unified view of an entity (all its
`extend`s), the outline and semantic colors.

It also gives **hints** (the editor's faintest severity), computed from the model alone,
without a database, about how the code meets the data:

| Where | Hint |
|---|---|
| `Entity.where/find/count/any/all/first/last/sum/min/max(…)`, `….sort_by(…).first()`, `for x in Entity.where(…)` | runs in SQL using an index (which one); in SQL but reading every row of the table (no index the schema infers covers the condition: references, `unique … when`, dates, display fields, fields compared with `==` in row rules); or in the engine over every record, or in SQL for part of the condition with the engine checking the rest (and why: a formula, a function, …); a method SQL has no equivalent for (`map`, `group_by`) runs on the rows SQL returns |
| a row rule `read … where …` | applied by the engine row by row when SQL cannot translate it (and why) |
| a formula | computed when read when it is not kept up to date by its changes (and why: it reads a whole entity, the clock, a function…) |

`POST /v1/dev/check` returns the same hints (`severity: "hint"`) to the web code editor.

## Interface files: `quadra apps`

### `quadra apps`

[Interface files](/reference/interfaces) are apps and saved views written in the project
(`interfaces/**/*.qdi`).

```bash
quadra apps pull "Sales cockpit"          # an app made in the client, as a file
quadra apps pull --all --to interfaces/   # every app and view made in the client
quadra apps push                          # install the project's files now
quadra apps new "Warehouse board" --entity Shipment
```

| Command | What it does |
|---|---|
| `pull <name\|id> \| --all [--to dir] [path] [--url URL]` | writes apps (in the directories of their folders, `sales-cockpit.qdi`; a name taken gets `-2`) and views (`views/<entity>.qdi`) made in the client as files; the data copies are archived: the files' take their place when the server starts. Shares with users and teams are not written (a warning says so) |
| `push [path] [--url URL]` | installs the project's interface files in the database now, as `quadra serve` and `quadra dev` do when they start (refused when the project does not check) |
| `new <"Name"> [--entity E] [--to dir] [path]` | a starting file: a title, and with `--entity` a count and a grid of the entity |

## Databases: `quadra db`

### `quadra db init`

```bash
quadra db init
quadra db init examples/extended --url sqlite:/tmp/demo.db
```

Creates the tables of the current model and commits the seeds as the first transaction.
Existing migrations count as applied. If the seeds fail, the database is left empty.

```text
✓ initialized sqlite:/tmp/demo.db: 8 tables, seeds committed
```

### `quadra db status`

```text
sqlite:/…/shop/.quadra/data.db
  schema      640eaafd35d6548c (CHANGED: run `quadra db migrate`)
  migrations  1 pending: customs
  commits     1
  last        6e84d0e45d3d8453188dc36e91c40e2bfbf15108b03373eea5c2c18850cdfd74
```

The fingerprint of the model's schema (and whether the database has it), the pending
migrations, the number of commits and the hash of the last one.

### `quadra db migrate`

```bash
quadra db migrate --dry-run
quadra db migrate
```

Runs the pending migrations, fills new fields from their defaults, checks the rules the new
model adds or changes on the existing records, and changes the tables that changed (printing
each step: `add column order.channel`, `rename column customer.name → title`, the indexes
created and dropped), in one transaction. `--dry-run` does all of it and writes nothing.
When there is nothing to do:
`schema up to date, no pending migrations: nothing to migrate`.
See [Migrations and changes](/reference/migrations-and-changes#how-a-migration-is-applied).

### `quadra db verify`

```text
✓ hash chain intact, tables match the history (26 records)
```

Recomputes the hash chain of the commit log, and compares every column of every table
(formulas included) with the values rebuilt from the history. Each problem is printed with
`✗` (altered history, missing commits, tables changed by hand), and the exit code is 1.

It reads the database a page at a time — the commits with what each wrote, on a connection
of its own, while each table is checked a page of rows at a time against their records'
history, the page's formulas computed — so its memory does not grow with the database. The
tables are cut into ranges of ids checked by several workers at once (half the cores, at most
6; `QUADRA_VERIFY_WORKERS=n` to choose), each with its own connection: 300–450 MB with 6
workers, and 20–30 s for 1.1 million records. A page's records are rebuilt from their
history; what their formulas read (lines, references) is read from its rows, which are
themselves checked on their own pages. When some rows or formula values are found wrong, the
check is done again reading those records from their history and with the values computed
for them, until nothing changes: the findings are those of computing every formula from the
history.

### `quadra db provisional`

```text
  Product#26.hs_code: To classify (until 2027-01-31)
```

The [provisional values](/reference/migrations-and-changes#provisional-values) left by
migrations, with their reason and deadline (`EXPIRED on …` once past it), or
`no provisional values`.

### `quadra db sweep`

Re-evaluates the rules that read the clock on every record: automatic transitions,
`becomes` reactions, postings, checks. With a database, rules are otherwise evaluated only
where data changed, so a rule like `when due < today()` needs this periodic pass.
`quadra serve` runs it every hour (`--sweep-every`). Prints
`✓ time-driven rules applied in transaction N`, or `✓ nothing changed` (and then nothing is
written: no empty commit).

### `quadra db fixture`

```bash
quadra db fixture italy examples/extended --url sqlite:/tmp/demo.db
```

Loads a test [fixture](/reference/testing#fixtures) (with the fixtures it extends) into the
database, in one transaction: demo and development data.
`✓ fixture italy loaded in transaction 2`.

### `quadra db import`

```bash
quadra db init examples/stress --url sqlite:/tmp/big.db
quadra db import ./data examples/stress --url sqlite:/tmp/big.db
```

Loads many records at once from CSV files, one per entity, named after it
(`Customer.csv`, `OrderLine.csv`): an initial load, or data from another system. The result
is the database the engine would have written creating the same records one by one: the
records are checked against every rule of the model, their formulas are computed and
materialized, the history is signed in the commit log, and `quadra db verify` passes. It is
written in one database transaction, in a few large commits (`--commit-size` records each,
default 250 000), with multi-row inserts on SQLite and `COPY` on PostgreSQL: nothing is
written if anything is wrong. The writes run on a thread of their own while the engine
computes: the history once the defaults are known, the projection rows once the formulas
are (while the rules are checked), each table's indexes once it is written; the phases
printed are those of the engine, `write` what is left after the rules.

```text
importing ./data into sqlite:/tmp/big.db
  read            0.7 s
  defaults        0.8 s
  formulas        9.5 s
  rules           4.4 s
  write           1.9 s
  + 200 Category
  + 5000 Product
  …
  + 415497 Shipment
✓ 1117364 records, 5500811 history entries, commits 2–6
```

```text
id,code,name,region,salesperson,tags,credit_limit
rossi,C1,Rossi Spa,north,giulia,VIP;NEW,5000.00 EUR
bianchi,C2,Bianchi Srl,south,paolo,,
```

| Rule | Detail |
|---|---|
| Header | The first line names the columns: stored fields of the entity, in any order, and optionally `id`. |
| `id` | A key for the row, used by the references of the other files (any text, unique in the file). It is not the record's id: records get new ids. |
| References | The `id` of a row of the target entity's file; `Entity:key` for a field whose type is a trait; `#123` for a record already in the database. Files may refer to each other in any order. |
| Lists and sets | Elements separated by `;` (`VIP;NEW`). |
| Values | `Int`, `Decimal`; `Percent` as `10%`; `Bool` as `true`/`false` (or `yes`/`no`, `1`/`0`); `Date` as `2026-03-02`; `DateTime` as `2026-03-02T09:00:00`; months as `2026-03`; `Money` as `12.50 EUR`, `Qty` as `3 pcs` (the unit may be left out when the type fixes it); `Price` as `1.50 EUR/pcs`; enum values and roles by name; text as it is. |
| Empty cell | No value: the field's `default` is computed, as when a record is created without it. |
| Flows | The state is given as it is (`Confirmed`): rows are the state of record, as in a migration. |
| Rules | Required values, `where`, `check` and `unique` on every record, records already in the database included; `warn` messages are attached; `frozen when` freezes the records it holds for. The first errors are reported with the file and line (`Order.date is required (Order row 12)`), broken `check`s with the records that break them. |
| Records already in the database | They are loaded too (the import is meant for large loads into a database that holds little): their formulas and warnings are recomputed with the new data. |
| Not supported yet | Entities that post to ledgers, `per company` fields, `record` values, defaults that number records (`next_number`) or create records. Reactions and automatic transitions do not run. |

The engine evaluates the rules and formulas over the data held in memory, about 0.5 to 1 KB
per record. `quadra bench gen` writes such files for the stress example of the repository
(`examples/stress`, see [below](#benchmarks-quadra-bench)).

### `quadra db views`

```bash
quadra db views export views.json --url sqlite:/tmp/demo.db
quadra db views import views.json --url sqlite:/tmp/other.db
```

The saved views, boards, apps, app folders and teams of a database ([Views and
boards](/reference/views-and-boards), [Apps and dashboards](/reference/apps-and-dashboards#folders))
as one JSON file (`"quadra_views": 2`), users by login and teams by name, so that they move to
another database whatever its ids. Each item has a `key` and its folder's `key` (`parent`).
`import` creates the teams that do not exist yet (by name), the folders parents first (one
already there with the same name at the same place is reused) and every view and app again
for its owner, in its folder, with its shares, in one transaction: nothing is written if a
login or a team is unknown. Files of the previous format (1, without folders) are read too.
The imported views are checked against the model when the server starts (renames applied,
what no longer fits marked broken).
`✓ 2 teams, 3 folders, 14 views and apps written to views.json` · `✓ 2 teams, 3 folders, 14 views and apps imported (checked against the model when the server starts)`.

## Data changes: `quadra change`

| Command | Effect |
|---|---|
| `quadra change list` | every `change` of the project, with its status and reason |
| `quadra change preview <name>` | runs it without writing: the values it would change, the records it would create, the certified metrics it touches |
| `quadra change apply <name> [--approved-by login]` | applies it, once, as its requester, with the approval of `login` |
| `quadra change revert <name> [--approved-by login]` | puts back the values it replaced, in a new transaction |

See [Migrations and changes](/reference/migrations-and-changes#changes).

## The server: `quadra serve`

```bash
quadra serve examples/extended --url sqlite:/tmp/demo.db --port 8091 --dev-login
quadra serve . --web web/dist --editors admin,marco
```

Serves the [HTTP API](/reference/http-api) of one project and its database on
`127.0.0.1`, and optionally the web client.

The values of the variables of `quadra.toml` come from the process's environment, then from a
`.env` file at the project root; a variable without a default and without a value stops it
from starting. The events with an [`outbound`](/reference/connections#outbound) are delivered
after their commit ([Variables and connections](/reference/connections)).

| Flag | Default | Effect |
|---|---|---|
| `--url URL` | see above | the database |
| `--port N` | `8080` | the port |
| `--web dir` | the client built into the binary | serves the web client from `dir` instead (any unknown path gets `index.html`) |
| `--dev-login` | off | enables `POST /v1/auth/dev-login`: a session for any user by login, without a password. Development only. |
| `--secure-cookies` | off | marks the session cookie `Secure` (needed behind HTTPS) |
| `--cache-mb N` | `256` | the size of the shared cache of records |
| `--prewarm-days N` | — | at start, loads into the cache the records created in the last `N` days |
| `--sweep-every secs` | `3600` | how often to run `db sweep`; `0` disables it |
| `--editors a,b` | none | the users allowed to edit the project's code from the browser (`*`: everyone; the project must be a git repository) |
| `--job-timeout secs` | `120` | the time limit of a request; `0` removes it. On PostgreSQL it is also the limit of each statement |
| `--max-rows N` | none | the rows a single query may read |
| `--max-list N` | none | the elements a list or a set may have while a request runs |
| `--readers N` | `2` | engines that serve the reads (grids, records, searches, reports, the formulas of apps) at the same time as the writer and each other; `0`: one engine does everything. Each has its own cache of `--cache-mb` |
| `--writers N` | `1` | engines that write; PostgreSQL only (SQLite takes one writer). Today more do not help: commits that update the same totals meet at the end and run again |
| `--max-queue N` | `1000` | the requests that may wait for the engine; past it a request is refused at once with `503` and `Retry-After: 1`. `0` removes the limit |

It prints `quadra: serving on http://127.0.0.1:8080` and runs until stopped.

### Limits of a request

The writes of a project are served one at a time, in the order they arrive; the reads by
`--readers` engines at once, each reading one moment of the database (what was committed
when it started). A slow request holds up only its engine. A request that goes past a
limit stops with an error (status `422`), and the server goes on serving the others:

| Limit | Error |
|---|---|
| time (`--job-timeout`): formulas, filters, actions and the SQL they run | `timeout: the request took longer than its time limit …` |
| rows (`--max-rows`) | `too many rows: a query read more than N rows …` |
| elements (`--max-list`) | `too many elements: a list grew beyond N elements …` |
| nested calls of formulas, functions and actions: 200 | `recursion too deep: more than 200 nested calls …` |

Sweeps and the editor's tests, commits and schema changes have an hour. A request that meets
an internal error answers `500`; the server throws away its cache and connections and goes
on.

## API tokens: `quadra token`

```bash
quadra token create giulia --name ci --read-only --days 30
quadra token revoke ci
```

```text
token `ci` for giulia (read-only), valid 30 days:
qd_t_115254e181f52a77c6979e4a1c28538874ed5609a14b59b2a96df4e659660dec
it is shown only now: store it in a secret manager
```

| Command / flag | Default | Effect |
|---|---|---|
| `token create <login>` | | a token that acts as that user (`` no user `x` `` if the login does not exist) |
| `--name n` | `<login>-<timestamp>` | the name, used to revoke it |
| `--read-only` | off | the token cannot write (`403 this token is read-only`) |
| `--days N` | `90` | the validity |
| `token revoke <name>` | | revokes every credential with that name: `1 credential(s) revoked` |

Only a SHA-256 hash of the token is stored in the database.

## Events: `quadra outbox`

The events emitted with [`emit`](/reference/flows-and-actions#the-outbox) are written to the
database's outbox with the commit of their transaction. Those with an
[`outbound`](/reference/connections#outbound) are delivered by the server (`quadra serve`,
`quadra dev`); the others wait for an integration that reads them, hands them on and marks
them delivered.

```bash
quadra outbox list --pending
quadra outbox list --pending --json
quadra outbox delivered 1 2
quadra outbox retry 7
```

```text
#1     tx 2      2026-11-22T09:00:00  SendEmail  {"document":{"ent":"SalesOrder","ref":22},"template":"quotation","to":"anna@example.com"}  (pending)
{"attempts":0,"created_at":"2026-11-22T09:00:00","delivered_at":null,"error":null,"event":"SendEmail","failed_at":null,"fields":{…},"id":1,"next_at":null,"tx":2}
✓ 2 event(s) marked delivered
```

| Command / flag | Effect |
|---|---|
| `outbox list` | every event, oldest first, with `pending` (and its attempts, the time of the next and the last error), `delivered <time>` or `failed <time>: <error>` |
| `--pending` | only the events neither delivered nor failed |
| `--json` | one JSON object per line: `id`, `tx`, `event`, `fields`, `created_at`, `delivered_at`, `attempts`, `next_at`, `error`, `failed_at` |
| `outbox delivered <id…>` | marks the events delivered (those already delivered are left as they are): `✓ N event(s) marked delivered` |
| `outbox retry <id…>` | makes events that failed for good pending again: the server tries them at its next round |

Delivery is at least once: if the integration stops after handing an event on and before
marking it, it sends it again, so receivers should ignore an `id` they have already seen.
An integration may also read the table `_quadra_outbox` directly
([Storage](/reference/storage)).

## Benchmarks: `quadra bench`

```bash
quadra bench gen --scale small --out /tmp/stress-small
quadra bench run examples/stress --url sqlite:/tmp/stress.db --scale small --json small.json
quadra bench run examples/stress --url "postgres://quadra@127.0.0.1:5432/bench#schema=stress" --scale medium
```

The stress benchmark of `examples/stress` (regions, customers, orders, lines, shipments:
relations four levels deep, rollups at every level, checks and permissions through them).
`gen` writes deterministic data as CSV files for `quadra db import`: the same `--seed`
(default 1) gives the same files. `run` starts from an empty database (it removes the
SQLite file, or drops the PostgreSQL schema named by `#schema=`, which it requires), loads
the data with the bulk import, then measures each scenario: grid pages of the API as an
administrator and as a salesperson, filtered, sorted; opening records; commits; aggregations;
searches; the same questions in plain SQL; deploys of schema changes. It prints a table and,
with `--json`, writes every number.

| Scale | Rows | |
|---|---|---|
| `tiny` | ~2 000 | for tests |
| `small` | ~100 000 | |
| `medium` | ~1 000 000 | |
| `large` | ~5 000 000 | |

| Flag | Default | Effect |
|---|---|---|
| `--timeout secs` | `60` | each scenario runs in a process of its own, killed after this time and reported as `> N s` (with how many records it had loaded and SQL statements it had sent) |
| `--load-timeout secs` | `7200` | the same for the load |
| `--reps N` | `5` | warm repetitions after the first (cold) one; commits repeat at least 10 times |
| `--only a,b` · `--skip a,b` | | the scenarios to run (`--skip load` measures a database loaded before) |
| `--commit-size N` | `250000` | records per commit of the load |

The scenarios, the baseline numbers and what limits them are in `docs/SCALING.md`.

## Other commands: `quadra-<name>`

A command `quadra` does not know runs the program `quadra-<name>` from the `PATH`, if there
is one, with the rest of the arguments, as git and cargo do: `quadra cloud deploy` runs
`quadra-cloud deploy`. Its exit code is `quadra`'s.

## `quadra --version`

Prints the version: `quadra 0.1.0`. `-V` is the same.
