# Introduction

**Quadra is a programming language for business software.** You describe your business once —
the data, the money, the rules, the processes, who may do what — and Quadra gives you the
database, the HTTP API, a web app to work in, and a complete, verifiable history of every value.

Its motto is *i conti quadrano*: **the books balance**. Quadra is built so that they always do.

## Who it is for

- **Developers building ERPs, back offices and line-of-business apps**: orders, invoices,
  stock, purchasing, production, accounting, approvals. The kind of software where a wrong
  cent, a lost update or an unexplained number is a real bug.
- **Teams that are tired of writing the same layers again and again**: tables, migrations,
  ORM classes, validation, REST endpoints, permission checks, audit logs, admin screens.
- **People who need to trust the numbers**: accountants, auditors, managers. A Quadra model
  reads almost like a specification, and every value can explain where it comes from.

## The mental model

In most stacks, *the code is a program that manipulates a database*. In Quadra, **the code is
the model, and everything else is derived from it**:

| You write | Quadra gives you |
|---|---|
| `entity`, fields, relations | tables with foreign keys, `UNIQUE` and `CHECK` constraints (SQLite or PostgreSQL) |
| formulas (`total = subtotal + vat`) | values that are always up to date, also stored as columns for outside readers |
| `check`, `warn`, `where`, `required when`, `unique` | rules enforced at every commit, from every entry point |
| `flow` and `action` | state machines and atomic operations, exposed as API calls and buttons |
| `frozen when` | documents that stop changing once they are confirmed or posted |
| `ledger` and `post to` | append-only registers (stock, receivables, general ledger) with automatic reversals |
| `role` | permissions applied to every row and field, in the API and in the web client |
| `test`, `fixture` | fast, deterministic tests that run in memory or on a real database |
| `migration`, `change` | safe schema evolution and audited, approved bulk data changes |

There is no "database layer" and no "business layer" to keep in sync: they are the same text.

## A taste

```quadra
entity Order:
    customer: Customer
    lines: [OrderLine]
    state: enum(Draft, Confirmed, Shipped, Cancelled) default Draft

    subtotal = lines.sum(l => l.amount)
    vat = vat_summary.sum(v => v.vat)
    total = subtotal + vat + shipping

    frozen when state is not Draft except notes, cancel_reason

    flow state:
        Draft -> Confirmed                  by confirm
        Confirmed -> Shipped                by ship
        Draft | Confirmed -> Cancelled      by cancel

    action confirm:
        require lines.count() > 0, "An order needs at least one line"
        number = next_number(current.company, "SO")
        confirmed_at = now()


entity OrderLine:
    order: Order parent
    product: Product
    qty: Qty<product.uom> where > 0
    price: Price<EUR, product.uom> default product.price
    discount: Percent where 0%..50% default order.customer.discount

    amount = (qty * price * (1 - discount)).round()
```

A few things to notice — they are what makes Quadra different:

- **Money, prices and quantities are types.** `qty * price` multiplies a `Qty<kg>` by a
  `Price<EUR, kg>` and gives money. Adding euros to dollars, or kilograms to pieces, does not
  compile. Money is exact (no floating point), and you must say where you round.
- **`=` is a formula, `default` is a snapshot.** `amount` always follows the line;
  `price default product.price` copies the list price when the line is created, so a later
  price change does not rewrite old orders.
- **A confirmed order is a fact.** `frozen when` makes it read-only *and* makes its formulas
  see the world as it was when it was frozen: tomorrow's VAT rate does not change yesterday's order.
- **The state machine is declared, not coded.** `confirm` can only run in `Draft`;
  everything an action does happens in one transaction, or not at all.

The compiler checks names, types, units, currencies, optional values, formula cycles and
state machines before anything runs:

```text
error: cannot add Money<EUR> and Money<USD>
   ┌─ tests/types.test.qd:34:17
   │
34 │     let total = 7.50 EUR + 2.00 USD
   │                 ^^^^^^^^^^^^^^^^^^^
```

## Compared to "framework + ORM + SQL"

| | Framework + ORM + SQL | Quadra |
|---|---|---|
| Schema | SQL migrations, ORM classes, API serializers, form definitions — kept in sync by hand | one model; tables, API and UI are derived |
| Derived values | computed in code, cached, recomputed by jobs, sometimes stale | formulas, always current, materialized at every commit |
| Money and units | decimals or floats, currency in a separate column, conversions by convention | `Money<EUR>`, `Qty<kg>`, `Price<EUR, kg>` checked by the compiler; exact arithmetic |
| Validation | in the controller, in the form, in a trigger — often in only one of them | `check`, `where`, `unique`, `required when`: enforced on every write, from any entry point |
| Status changes | `if order.status == ...` scattered in the code | `flow` + `action`: only the declared transitions exist |
| Audit | an audit table someone remembers to write to | every value has a history and a cause chain (`why(record)`), in a hash-chained log |
| Permissions | middleware and query filters per endpoint | `role` rules on rows and fields, applied by the engine everywhere |
| Concurrency | last write wins, unless you add locking | optimistic concurrency: a commit on data changed by others is refused with who, what and when |
| Schema changes | a migration script that hopes the data is clean | `migration` blocks with dry run on the real data, provisional values and verification |

Quadra is not a low-code tool: it is a typed language with a compiler, a formatter, a language
server, tests and code review in git. It just happens to know what an invoice is.

## How a project runs

```text
 .qd files ──▶ quadra check ──▶ quadra test ──▶ quadra db init / migrate ──▶ quadra serve
 (the model)    names, types,     in memory or     tables, history,            HTTP API +
                units, cycles     on SQLite        signed commits              web client
```

The source of truth in the database is the **history**: one entry per field per transaction,
each commit chained to the previous one by a hash. The ordinary tables (`order`, `order_line`…)
are projections of that history, written in the same transaction, so any SQL tool can read
them — and `quadra db verify` can prove they were not tampered with.

## Where to go next

- [Installation](./installation): build the `quadra` command and set up your editor.
- [Quick start](./quick-start): a running app in five minutes.
- [Tutorial](/tutorial/): build a small shop step by step — customers, products, orders,
  invoices, stock, permissions, a database and an API.
