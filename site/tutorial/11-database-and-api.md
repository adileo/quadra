# Step 11: The database and the API

<SandboxLink project="tutorial-11" file="tests/fixtures.qd" />

**Goal:** run the shop for real: a database, demo data, the web client for the staff, and an
HTTP API for programs — an e-commerce site that sends orders, say.

The model doesn't change in this step. There is one new fixture, `demo`, to fill a development
database, and a test that keeps it honest.

::: warning Do this on your own machine
The Sandbox runs the compiler and the tests in your browser; the database and the server run
on your machine ([Installation](/guide/installation)). Work in the folder you've been using
for the tutorial: the next step migrates the database you create here.
:::

## Demo data

`tests/fixtures.qd` gets a fixture that builds on the others: an order shipped and invoiced
with a partial payment, a small order paid by card, and a draft.

```quadra
# Demo data for a development database: `quadra db fixture demo`.
fixture demo extends shop_with_orders:
    order.ship()
    let big_invoice = order.create_invoice()
    big_invoice.post()
    Payment(invoice=big_invoice, amount=100.00 EUR, method="bank transfer")

    let small = Order(customer=anna, lines=[
        OrderLine(product=sencha, qty=500 g),
        OrderLine(product=mug, qty=2 pcs),
    ])
    small.confirm()
    let small_invoice = small.create_invoice()
    small_invoice.post()
    Payment(invoice=small_invoice, amount=small_invoice.total, method="Card")

    Order(customer=anna, lines=[OrderLine(product=beans, qty=1 kg)])     # still a draft
```

(The payment methods are deliberately inconsistent — `"bank transfer"`, `"Card"` — as free
text tends to be. Step 12 cleans them up.)

A test makes sure the demo data stays valid as the model evolves:

```quadra
from tests.fixtures import demo


test "the demo data is consistent":
    use demo
    expect Order.count() == 3
    expect Invoice.count(i => i.state is Posted) == 2
    expect cafe.balance == 87.92 EUR
    expect anna.balance == 0 EUR
    expect beans.on_hand == 15 kg
```

## Create the database

```bash
quadra db init
```

```text
✓ initialized sqlite:/…/shop/.quadra/data.db: 22 tables, seeds committed
```

By default the database is SQLite in `.quadra/data.db`; use `--url` or a `[database]` section
in `quadra.toml` for another file or for PostgreSQL. Load the demo data:

```bash
quadra db fixture demo
```

```text
✓ fixture demo loaded in transaction 2
```

```bash
quadra db status
```

```text
sqlite:/…/shop/.quadra/data.db
  schema      3e62cc3b6e7630d4 (up to date)
  commits     2
  last        f3809e75e00beb2a2435038588e4108c84bb2460ed8f8503766badcd7aeb2212
```

### What's in the database

- **The history is the truth**: one entry per field per transaction (`_quadra_history`), the
  ledger rows, and the commits. Each commit records who, when and why, and a hash chained to
  the previous commit.
- **Ordinary tables are projections**: `customer`, `product`, `order`, `order_line`,
  `invoice`, `stock`… with foreign keys, `UNIQUE` and `CHECK` constraints, and a column for
  every formula. They're written in the same transaction, so any SQL tool or BI dashboard can
  read them.

`quadra db verify` recomputes the hash chain and compares every table with the history:

```bash
quadra db verify
```

```text
✓ hash chain intact, tables match the history (55 records)
```

If someone edits a table by hand, or removes a commit, `verify` tells you.

## Serve it

```bash
quadra serve --dev-login --web /path/to/quadra/web/dist
```

```text
quadra: serving on http://127.0.0.1:8080 (development login enabled)
```

(Add `--port 8091` to choose the port; `--url` to choose the database.)

### The web client

Open <http://127.0.0.1:8080> and sign in as `olivia`. On the left are your modules —
Catalog, Customers, Invoices, Orders, Receivables, Stock — with their entities and ledgers.
Each entity is a **sheet**: sortable, filterable with a Quadra expression (try
`state is Confirmed and total > 20.00 EUR` on Order), with the formulas as columns. Edits
are drafts until you press **Save**, which sends them as one transaction. Open a record to see
its fields, the actions you may run in its current state, its warnings and its history.

Now sign out and sign in as `sam`, the clerk: costs and margins are gone from the products,
and invoices are not in the menu.

### The HTTP API

Programs use tokens. Create one for `olivia`:

```bash
quadra token create olivia --name tutorial --days 30
```

```text
token `tutorial` for olivia, valid 30 days:
qd_t_d46350b8…
it is shown only now: store it in a secret manager
```

Only a hash of the token is stored. In the examples, `$TOKEN` is that value and the server
runs on port 8080:

```bash
TOKEN=qd_t_…
API=http://127.0.0.1:8080
```

**Who am I?**

```bash
curl -s $API/v1/me -H "Authorization: Bearer $TOKEN"
```

```json
{"csrf":null,"kind":"token","read_only":false,"user":"olivia","user_id":22}
```

**Query.** `POST /v1/query` with an entity, the fields you want (paths like `customer.name`
work), and optionally a filter and extra columns — written in Quadra and **type-checked**
like your source code:

```bash
curl -s $API/v1/query -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"entity": "Order",
       "fields": ["number", "customer.name", "state", "total"],
       "where": "state is Confirmed and total > 20.00 EUR"}'
```

```json
{
  "base": 2, "entity": "Order", "total": 1, "offset": 0,
  "fields": ["number", "customer.name", "state", "total"], "columns": [],
  "rows": [
    { "id": 49, "display": "SO/2026/00002", "version": 2, "frozen": true, "warnings": [],
      "cells": {
        "number":        { "v": "SO/2026/00002", "d": "SO/2026/00002" },
        "customer.name": { "v": "Anna Rossi", "d": "Anna Rossi" },
        "state":         { "v": { "variant": "Confirmed", "fields": [] }, "d": "Confirmed" },
        "total":         { "v": { "money": "46.30", "cur": "EUR" }, "d": "46.30 EUR" } } }
  ]
}
```

Every cell has a typed value `v` (money with its currency, quantities with their unit…) and a
display string `d`. `base` is the transaction the data was read at.

A filter that doesn't type-check is refused with the compiler's message:

```bash
curl -s $API/v1/query -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"entity": "Order", "fields": ["number"], "where": "total > 20.00 USD"}'
```

```json
{"diagnostics":["cannot compare Money<EUR> and Money<USD>"],"error":"the expression does not check"}
```

`columns` adds computed columns of your own, also in Quadra:

```bash
curl -s $API/v1/query -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"entity": "Customer", "fields": ["name", "balance", "tier"],
       "columns": {"owes_a_lot": "balance > 50.00 EUR"}}'
```

```text
Anna Rossi       balance 0.00 EUR    tier Standard   owes_a_lot false
Caffè Centrale   balance 87.92 EUR   tier Standard   owes_a_lot true
```

(Rows shortened for reading.)

**Write.** `POST /v1/tx` runs a list of operations as **one transaction**. Every write needs an
`Idempotency-Key` header: if the network fails and you retry with the same key, the server
answers with the first result instead of creating a second order.

```bash
curl -s $API/v1/tx -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: tutorial-order-1' \
  -d '{"ops": [{"op": "create", "entity": "Order", "ref": "o",
                "values": {"customer": 38,
                           "lines": [{"product": 30, "qty": "4 pcs"},
                                     {"product": 31, "qty": "250 g"}]}}]}'
```

```json
{"base":3,"created":{"o":67},"tx":3}
```

The same request again:

```json
{"created":{"o":67},"replayed":true,"tx":3}
```

Values are Quadra literals (`"4 pcs"`, `"7.50 EUR"`, `"2026-11-21"`), references are ids, lines
are nested objects. `250 g` is converted to the product's unit (kg); a quantity of the wrong
kind is refused.

To change existing records, send the `base` you read them at. Update and confirm in one
transaction:

```bash
curl -s $API/v1/tx -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: tutorial-confirm-1' \
  -d '{"base": 3, "ops": [
        {"op": "update", "entity": "Order", "id": 67, "values": {"notes": "Ring twice"}},
        {"op": "action", "entity": "Order", "id": 67, "action": "confirm"}]}'
```

```json
{"base":4,"created":{},"tx":4}
```

**The model says no.** Rules come back as `422` with the model's message:

```json
{"error":"OrderLine.qty = 0 pcs: does not satisfy its `where`"}
```

```json
{"error":"cannot convert kg to pcs"}
```

**Someone else changed it.** Two programs read Anna at transaction 4 and both change her email.
The first wins; the second gets `409` with who changed what, when and why — nothing is
overwritten silently:

```json
{"conflicts":[{"at":"2026-11-22T00:11:26",
               "cause":"API request tutorial-email-1  (olivia, 2026-11-22 00:11)",
               "tx":5,"user":"olivia","what":["Customer#38.email"]}],
 "error":"changed by others since you read it: review their changes and try again"}
```

**Permissions apply.** With a read-only token for `sam` (`quadra token create sam --read-only`),
hidden fields come back hidden, other entities are forbidden, and writes are refused:

```json
"cells": {"code": {"v": "MUG-01", "d": "MUG-01"}, "price": {"d": "7.50 EUR/pcs", …},
          "cost": {"hidden": true}, "margin": {"hidden": true}}
```

```json
{"error":"permission denied: cannot read Invoice"}
```

```json
{"error":"this token is read-only"}
```

**Records and history.** `GET /v1/records/Order/67` returns every field, the fields you may
edit now and the actions you may run now; `GET /v1/records/Customer/38/history` returns every
write with its transaction, time, user and cause; `GET /v1/stream` sends a server-sent event
for each commit. The full list is in the [API reference](/reference/http-api).

## The code editor

`quadra serve … --editors olivia` lets the listed users edit the project from the browser
(the **Code** section of the web client), when the project is a git repository. A commit
there is a release: check, tests, a dry run of the new model on the live data, `git commit`,
then the tables are brought to the new model (only what changed) and the server reloads it.

## Try it

1. Query the products with `on_hand`, `reserved` and `available`.
2. Create an order through the API for 100 mugs for Anna, then try to confirm it: what does the
   API answer?
3. Ship order 67 as `wes` (create a token for him) — and then try to create an order as `wes`.

## Recap

- `quadra db init` creates the tables; `quadra db fixture` loads demo data; `db status` and
  `db verify` tell you where you are and prove the history is intact.
- `quadra serve` runs the API and the web client; `--dev-login` is for development, tokens are
  for programs.
- Queries and filters are Quadra expressions, type-checked; writes are transactions with
  idempotency keys and optimistic concurrency; the model's rules and permissions apply to every
  request.

[Next: migrations and changes →](./12-migrations-and-changes)
