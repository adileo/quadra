# Quick start

Five minutes from an empty folder to a running app — database, API, web app and code editor —
that updates every time you save. You need the `quadra` command ([Installation](./installation)).

<SandboxLink project="quick-start" file="main.qd" label="Or try it in the browser first" />

## 1. Create a project

```bash
mkdir hello && cd hello
quadra init
```

`quadra init` asks three questions; press Enter to accept the defaults:

```text
  Quadra · new project in /Users/you/hello

  Project name (hello)
  Database: sqlite or postgres? (sqlite)
  Create a git repository (the browser code editor commits to it)? [Y/n] (y)

  ✓ hello: quadra.toml, main.qd, tests/main.test.qd, git repository
  database: SQLite in .quadra/data.db

  Next: quadra dev
```

The name defaults to the folder's. SQLite needs nothing installed (the data lives in
`.quadra/`); choose `postgres` to give a PostgreSQL URL instead. Without a terminal, or with
`--yes`, the defaults are taken: `quadra init --name hello --db postgres://localhost/hello --yes`.

You get a small but complete model in `main.qd`:

```quadra
entity Customer:
    name: Text
    email: Text?
    credit_limit: Money<EUR> default 1000.00 EUR
    orders: [Order] via customer

    # a rollup across relations: always up to date
    open_total = orders.where(o => o.state is Confirmed).sum(o => o.total)

entity Product:
    name: Text
    price: Money<EUR>

entity Order:
    customer: Customer
    lines: [OrderLine]
    state: enum(Draft, Confirmed) default Draft

    total = lines.sum(l => l.subtotal)

    check total <= customer.credit_limit, "Over the credit limit"

    flow state:
        Draft -> Confirmed by confirm

entity OrderLine:
    order: Order parent
    product: Product
    qty: Int where > 0 default 1
    subtotal = (product.price * qty).round()
```

- every `entity` is a table, an API resource and a sheet in the web app;
- `open_total` and `total` are **formulas**: always up to date, never stored by hand;
- `check` is a rule: a write that breaks it is refused, with its message;
- `flow state` is the life of an order: `confirm` moves it from `Draft` to `Confirmed`;
- `OrderLine` has a `parent`: lines belong to their order (and are edited inside it).

And `tests/main.test.qd`, with a `demo` fixture (sample data) and two tests.

## 2. Run it

```bash
quadra dev
```

```text
  ✓ new database sqlite:/Users/you/hello/.quadra/data.db: 11 tables
  ✓ demo data: fixture `demo`
  ✓ user `dev` to sign in with

  Quadra dev · hello
  ➜ App          http://127.0.0.1:8080
  ➜ Code editor  http://127.0.0.1:8080/#/~code
  watching /Users/you/hello: save a file (here or in the browser) and the app updates
```

`quadra dev` created the database, loaded the `demo` fixture and a user, and serves the web app
and the code editor. Open <http://127.0.0.1:8080>: you are signed in, with a sheet per entity.
Add a product, open the order (⌘/Ctrl-click a row) and watch `total` follow its lines.

::: tip Another port
If 8080 is taken, `quadra dev` picks the next free port and prints it. `--port 9000` chooses one.
:::

## 3. Change the model while it runs

Keep the app open. In `main.qd`, give customers a phone number and save:

```quadra{3}
entity Customer:
    name: Text
    phone: Text?
    email: Text?
```

The terminal says `quadra dev: applied ✓` and the app shows the new **Phone** column — no
restart, no migration to write: the database was updated in place, and every open page reloaded
its sheets.

Now make a mistake on purpose — `phone: Text? default 5 EUR` — and save. Nothing breaks: the
app keeps running the last version that compiled, and shows what is wrong:

```text
The saved code has 1 error — the app runs the last version that compiled.
main.qd:4 cannot store Money<EUR> in a default of type Text?
```

Put it back and the message goes away. The same happens when the **data** does not fit the new
code. Add a required field, `vat: Text`, and the existing customer has no VAT number:

```text
Not applied: the data does not fit the saved code — the existing data does not satisfy the new
model: Customer.vat is required
```

Give it a default (`vat: Text default "n/a"`) and it applies, filling the existing rows. To
rename a field and keep its values, add a [migration](/reference/migrations-and-changes) next
to the change; `quadra dev` runs it as soon as it is saved. And when you'd rather start over:
`quadra dev --reset` recreates the SQLite database from the `demo` fixture.

You can edit in your own editor (with the [extension](./installation#editor-support-vs-code-and-cursor)) or in
the **code editor** of the app (<http://127.0.0.1:8080/#/~code>): it saves to the same files,
follows changes made on disk, and commits to git when you are happy with a version.

## 4. Test it

```bash
quadra test
```

```text
✓ an order adds up its lines
✓ the credit limit is a rule

2 passed, 0 failed
```

Tests run in memory, in milliseconds, against a fresh copy of the model.

## 5. Talk to the API

The same data is available over HTTP. Programs authenticate with a token:

```bash
quadra token create dev
```

```text
token `dev-20261122001512` for dev, valid 90 days:
qd_t_a573c66b…
it is shown only now: store it in a secret manager
```

Every write carries an `Idempotency-Key`, so a retry never creates a record twice:

```bash
TOKEN=qd_t_…
curl -s http://127.0.0.1:8080/v1/tx \
     -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
     -H 'Idempotency-Key: first-product' \
     -d '{"ops":[{"op":"create","entity":"Product","values":{"name":"Teapot","price":"24.00 EUR"}}]}'
```

See the [HTTP API reference](/reference/http-api) for queries, records and live updates.

## What just happened

From two commands you got:

- a type-checked model with a test suite;
- a database with real tables, constraints and a hash-chained history of every value
  (`quadra db verify` proves it is intact);
- an authenticated, permission-checked JSON API;
- a web app and a code editor that follow every change you save.

::: warning `quadra dev` is for your machine
It signs you in without a password, lets everyone edit the code and applies every saved file.
In production use [`quadra serve`](/reference/cli#the-server-quadra-serve): changes go through
commits, the dry run on the live data and a deploy.
:::

## Next

The [tutorial](/tutorial/) builds a real small business application step by step: customers,
products with prices per kilogram, orders, invoices and payments, stock, permissions, and the
database operations you need in production.
