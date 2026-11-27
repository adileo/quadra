# Testing

Tests are written in Quadra, next to the model. They run in memory in a fraction of a
second, or on a real database, and they can also check that wrong code is refused by the
compiler.

```quadra
# tests/orders.test.qd (the model is the minimal example: Customer, Product, Order, Stock)
from tests.fixtures import shop

test "an order is confirmed and shipped":
    use shop

    let o = Order(customer=anna, lines=[OrderLine(product=pen, qty=4 pcs, discount=10%)])
    expect o.total == 5.40 EUR

    o.confirm()
    expect anna.open_total == 5.40 EUR

    as carla: o.ship()
    expect pen.stock == 6 pcs

    let big = Order(customer=anna, lines=[OrderLine(product=pen, qty=7 pcs)])
    big.confirm()
    expect_error "Not enough stock":
        big.ship()
    expect big.state is Confirmed            # the failed transaction was undone
```

## `test`

`test "name":` followed by statements. A test can be written in any file; by convention they
live in `tests/`, where every declaration of the project is visible without imports
([Projects and modules](/reference/projects-and-modules#scripts-tests-and-migrations)).

| Rule | Detail |
|---|---|
| Isolation | Every test starts from an empty database with the `seed` blocks applied. |
| Transactions | Every top-level statement is one transaction, like one click in the application: it is committed, or undone entirely if it fails. |
| Variables | `let` variables live until the end of the test. |
| Clock | `2026-01-01 09:00` until a `clock` statement changes it. |
| User | none: permissions are not checked, until `as` or `login()`. |
| Failure | The first failing statement ends the test; the message shows the file and line. |

All the [statements](/reference/expressions#statements) of actions are available, plus:

| Statement | Purpose |
|---|---|
| `expect cond` | the condition must hold |
| `expect_error "text": …` | the block must fail with a message containing `text` |
| `expect_compile_error "text": …` | the block must be refused by the compiler with a message containing `text` |
| `use fixture` | loads a fixture |
| `clock time` | sets the clock |
| `as login: …` | runs with a user's permissions |
| `receive Inbound "…"`, `receive Inbound file "…"` | a message comes in ([Integrations](#integrations)) |
| `expect sent Event(field=…)`, `expect not sent …` | an event was emitted, or was not |
| `deliver Event`, `deliver Event reply R(…)`, `deliver Event failed "…"` | the other side answers |

## `expect`

```quadra
expect so.untaxed == 30.00 EUR
expect inv.installments.map(i => i.amount) == [18.30 EUR, 18.30 EUR]
expect GL.where(g => g.account == receivables and g.partner == rossi).sum(g => g.amount) == 0
```

When the condition is a comparison, a failure shows both sides:

```text
✗ expect fails  (main.qd:32)
    main.qd:33: expectation failed: (1 + 1) == 3
      left:  2
      right: 3
```

## `expect_error`

```quadra
expect_error "Credit limit exceeded":
    so.confirm()
expect so.state is Draft                  # the failed transaction was undone
expect so.deliveries.count() == 0         # including what the reactions did
```

The block's statements run one by one, each as its own transaction, until one fails. The
test continues if the failure message contains the text; otherwise it fails with
`expected an error containing "…", got: …`, or `expected an error containing "…", but it succeeded`.
Statements of the block that succeeded before the failure stay committed.

## `expect_compile_error`

```quadra
test "the compiler refuses type errors":
    expect_compile_error "cannot add Money<EUR> and Money<USD>":
        let x = 10.00 EUR + 5.00 USD
    expect_compile_error "now() is not allowed in a formula":
        extend StockMove:
            age = now() - date
    expect_compile_error "missing Cancelled":
        extend SalesOrder:
            label: Text = match state:
                Draft     => "Draft"
                Sent      => "Sent"
                Confirmed => "Confirmed"
                Locked    => "Locked"
```

The block is not run. Its source is added to a copy of the project and checked:

- a block that starts with `extend`, `fn`, `patch`, `entity` or `record` is checked as a
  top-level declaration;
- anything else is checked as the body of a function.

The test passes if one of the errors contains the text. Otherwise:
`expected a compile error containing "…", but it compiles`, or `…, got: <the errors>`.

The rest of the project ignores what is inside these blocks, so they can mention undefined
names or wrong types.

### `patch`

`patch Entity:` replaces the definition of fields of an existing entity. It exists for
`expect_compile_error`, to show that a different definition would be refused:

```quadra
expect_compile_error "formula cycle":
    patch StockMove:
        unit_cost: Price<company.currency, product.uom> = product.cost[company].round()
```

## Fixtures

```quadra
# tests/fixtures.qd
fixture shop:
    clock 2026-11-02 09:00
    User(login="admin", roles={Admin})
    User(login="carla", roles={Clerk})
    let anna = Customer(name="Anna Bianchi")
    let pen = Product(code="PEN", name="Pen", price=1.50 EUR/pcs)
    Restock(product=pen, qty=10 pcs)

fixture big_shop extends shop:
    let vip = Customer(name="VIP", credit_limit=50_000.00 EUR)
    Restock(product=pen, qty=1_000 pcs)
```

`fixture name [extends other]:` declares shared test data.

| Rule | Detail |
|---|---|
| One transaction | A fixture runs as a single transaction, so its records may refer to each other freely. |
| System | It runs without permission checks. |
| `extends` | The fixtures it extends run first, in the same transaction. |
| Variables | Its `let` variables become variables of the test that uses it (`anna`, `pen`, and with `big_shop` also `vip`). |
| `clock` | Sets the clock for the fixture and for the rest of the test. |

`use name` loads a fixture into the test. A fixture declared in another file of `tests/` must
be imported: `from tests.fixtures import shop`.

The roles used above are declared in the model:

```quadra
role Admin:
    all *

role Clerk:
    read Order, Customer, Product
    create, edit Order where o => o.customer.name != "VIP"
    run Order.ship
```

```quadra
test "a clerk cannot open orders for VIP customers":
    use big_shop
    let o = as carla: Order(customer=anna)
    expect_error "permission denied: carla cannot create Order":
        as carla: Order(customer=vip)
```

`quadra db fixture <name>` loads a fixture into a real database, for demos and development
([CLI](/reference/cli#quadra-db-fixture)).

## `clock`

```quadra
clock 2026-11-20 10:00
clock 2026-12-01            # a date: 09:00 that day
```

Sets the time that `now()` and `today()` return, that defaults read, that records and ledger
rows are stamped with, and that exchange rates are looked up at.

Then it runs the rules that read the clock on every record, as the server's sweep does when
their time comes: a `becomes now() >= shipped_at + 24 hours` that the new time makes true
fires at the `clock` statement, in a transaction of its own, before the statements that
follow ([Waits, joins, timeouts](/reference/flows-and-actions#waits-joins-timeouts-and-cancellations)).

```quadra
clock 2026-03-03 08:59
expect o.state is Shipped
clock 2026-03-03 09:00                 # 24 hours after shipping
expect o.state is Completed
```

## As a user

```quadra
as anna:
    for m in delivery.moves:
        m.reserve()
    delivery.validate()

as anna: inv.post()

let inv = as marco: so.create_invoice()
```

`as login:` runs a block, a single statement, or an expression with the permissions of that
user. An unknown login fails the test: `` unknown user `nobody` ``.

`login("admin", company=acme)` sets the current user, and optionally the current company, for
the statements that follow (in a test or a fixture). It is how `current.user` and
`current.company` get a value in tests.

See [Security](/reference/security#testing-permissions) for permission tests.

## Integrations

Tests send nothing and receive nothing: they play the other side.

```quadra
test "an order from the shop is fulfilled and tracked":
    receive OrderIn file "tests/shop/order-1001.json"     # as if the shop sent it
    receive OrderIn file "tests/shop/order-1001.json"     # the same again: a duplicate, nothing happens
    let o = Order.where(x => x.number == "#1001").first() ?? fail "no order"
    expect sent OrderPlaced(order=o)
    deliver OrderPlaced reply ErpAck(tracking="TRK-1")  # the ERP accepted it, with this answer
    expect o.tracking == "TRK-1"

test "a bad message changes nothing":
    expect_error "a negative total":
        receive OrderIn "{\"id\": \"7\", \"name\": \"#7\", \"total_price\": \"-1\"}"
    expect not sent OrderPlaced

test "a refusal is handled":
    let o = Order(number="A1", total=1)
    deliver OrderPlaced failed "503: busy"
    expect o.state is OnHold
```

| Statement | What it does |
|---|---|
| `receive Inbound "…"` | the text is read as the inbound's record and its code runs, as the system, in a transaction of its own, as when the message comes in. A message with a key already received is a duplicate: nothing happens. One that is not the record, or whose code fails, fails the statement with `rejected: …` (in production it would be `rejected`) |
| `receive Inbound file "path"` | the same with the text of a file, from the project's folder (a message saved from the inbox: `quadra inbox show <id>`) |
| `expect sent Event(field=v, …)` | an event of that kind with those fields was emitted by a committed transaction (the other fields whatever they are); `expect sent Event`, any of them. On failure, the message lists those sent |
| `expect not sent Event(…)` | none was |
| `deliver Event` | each event of that kind still waiting is delivered: its `on Event delivered` reactions run, each in a transaction of its own |
| `deliver Event reply R(…)` | the same, with the answer the outbound's `reply` reads (`d.reply`) |
| `deliver Event failed "error"` | each fails for good: its `on Event failed` reactions run, with `f.error` |

A `deliver` with no event waiting fails (`no OrderPlaced waiting to be delivered`). What the
code of a `publish` and of a delivery's reaction writes is not published back, as in
production ([`publish`](/reference/connections#publish-keeping-a-record-in-step-with-another-system)).

## Variables

Tests do not read the environment: `env.NAME` is the variable's `test` value in `quadra.toml`,
else its default, and the events with an `outbound` are not sent
([Variables and connections](/reference/connections#in-tests)).

## Running tests

```bash
quadra test                         # every test of the project in the current folder
quadra test examples/extended       # a project elsewhere
quadra test --filter invoice        # tests whose name contains "invoice"
quadra test --db sqlite             # each test on a fresh SQLite database
quadra test --db postgres://me@localhost:5432/quadra_test
```

With the `run Order.ship` rule missing from `Clerk`, the run reports:

```text
✓ order: confirm, ship, stock
✓ the compiler rejects type errors
✗ an order is confirmed and shipped  (tests/orders.test.qd:3)
    tests/orders.test.qd:12: permission denied: carla cannot run Order.ship
✓ a clerk cannot open orders for VIP customers

3 passed, 1 failed
```

The project is checked first; if it has errors, they are printed and no test runs. The exit
code is 0 when every test passes.

With `--db`, every test gets a new database (a temporary SQLite file, or a new schema on
PostgreSQL), statements load what they need from it as an API request would, and at the end
the data is reloaded from the database, compared with the engine's, and verified
(`quadra db verify`).

## Golden files for analyses

The output of the analyses (`quadra trace`, metric fingerprints) is plain text and stable, so
it can be kept in a file and compared in continuous integration to notice any change in what
a value depends on. The extended example keeps `tests/trace.expected`, the output of
`quadra trace` for a few fields and of the metric fingerprints; the Quadra repository
regenerates it with `UPDATE_GOLDEN=1 cargo test -p quadra-analysis --test golden`.
`quadra test` itself does not read these files.
