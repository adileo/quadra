# Reference

The reference describes the Quadra language and its tools exactly as the compiler
implements them. Every example on these pages has been type-checked, and most have been run,
with the `quadra` command-line tool.

If you are new to Quadra, start with the guide and the tutorial. Use the reference when you
need the exact syntax of something, the full list of methods, or what an error message means.

## The language

| Page | What it covers |
|---|---|
| [Projects and modules](/reference/projects-and-modules) | `quadra.toml`, files and module names, `from … import …`, the standard library, the `tests/` and `migrations/` folders |
| [Syntax](/reference/syntax) | Indentation, line continuation, comments, keywords, every kind of literal (`5.40 EUR`, `3 pcs`, `1.50 EUR/pcs`, `22%`, `2026-11-21`, `30 days`), f-strings |
| [Entities](/reference/entities) | `entity`, `record`, `trait`, `extend`, `enum`, fields and their modifiers, relations, formulas and defaults, temporary entities, seeds, functions, events |
| [Types](/reference/types) | Every type, with currencies and units: how amounts are added, multiplied, converted, rounded or refused |
| [Expressions](/reference/expressions) | Operators, `?.` and `??`, `if … else`, `when:`, `match`, lambdas, statements, every collection, text, date and amount method, the built-in functions |
| [Rules](/reference/rules) | `check`, `warn`, `unique`, `required when`, `where`, `frozen when … except … live …`, defaults versus formulas |
| [Flows and actions](/reference/flows-and-actions) | State machines, actions, `require`, `fail`, events, `emit` and the outbox, reactions (`on … enters`, `on … becomes`, `on … created`, `on … changes`, `before …`) |
| [Ledgers](/reference/ledgers) | Append-only ledgers, `key` / `measure` / `attr`, `post to`, automatic reversal, per-group checks, queries |
| [Security](/reference/security) | Roles, permission rules, row conditions, hidden fields, separation of duties |

## Working with a project

| Page | What it covers |
|---|---|
| [Testing](/reference/testing) | `test`, `fixture`, `use`, `clock`, `as user:`, `expect`, `expect_error`, `expect_compile_error`, `patch` |
| [Migrations and changes](/reference/migrations-and-changes) | Schema migrations (`rename`, `convert`, `backfill`, `verify`, provisional values, CSV staging tables) and governed data changes |
| [Metrics](/reference/metrics) | Certified metrics and controls, fingerprints, `metrics.lock` |
| [Views and boards](/reference/views-and-boards) | Saved views and kanban boards made in the web client: sharing with users, teams, roles or everyone, teams versus roles, moving cards through transitions, what a deploy does to them |
| [Apps and dashboards](/reference/apps-and-dashboards) | Apps made in the web client: a canvas of components, spreadsheets whose formulas are Quadra expressions over the database (cell references, ranges, other sheets, spill), Markdown boxes, sharing, Excel export |
| [CLI](/reference/cli) | Every `quadra` command and flag |
| [HTTP API](/reference/http-api) | Authentication, queries, transactions, idempotency and conflicts, live events, the code-editor endpoints |
| [Storage](/reference/storage) | The signed commit log, the SQL tables, loading on demand, optimistic concurrency, SQLite and PostgreSQL |

## Conventions used here

- Code blocks marked `quadra` are Quadra source. Small fragments (a single field, a single
  expression) are written as they appear inside an entity or a function.
- Many examples are excerpts of the two example projects of the repository:
  `examples/minimal` (customers, products, orders, stock, in one file) and
  `examples/extended` (a complete ERP: sales, purchasing, inventory, manufacturing,
  accounting, invoicing). Their messages are translated here; the code is the same.
- Error and warning messages are quoted exactly as `quadra check` and `quadra test` print
  them.
- `::: warning` boxes mark behaviour that is accepted by the compiler but does not (yet) do
  what the syntax suggests. Read them before you rely on the feature.
