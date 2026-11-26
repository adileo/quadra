# Step 8: Tests

<SandboxLink project="tutorial-08" file="tests/fixtures.qd" />

**Goal:** you've been writing tests since step 1. Now the suite is big enough to deserve some
structure: shared test data, control over time, tests for what must fail, and tests for what
must not even compile.

## How tests run

- A `test "name":` block starts from an **empty world**: only the seeds (the standard
  library's currencies and units, and your own `seed:` blocks) exist.
- **Every top-level statement is one transaction**, like a click on Save. If it fails, it is
  rolled back.
- Tests run **in memory** by default: a few milliseconds each.
- Without `as user:` (next step), a test runs as the system: permission rules are skipped.

## Fixtures: shared test data

A `fixture` is a named block of setup code. It runs as **one transaction**, so its records can
refer to each other freely. After `use shop`, the test sees every `let` of the fixture as a
local name. Fixtures live in `tests/fixtures.qd`:

```quadra
# Shared test data. A fixture runs as ONE transaction, so its records can refer to each other.
# After `use shop`, a test sees every `let` below as a local name: mug, beans, anna…

fixture shop:
    clock 2026-11-02 09:00

    let mugs = Category(name="Mugs")
    let coffee = Category(name="Coffee")
    let tea = Category(name="Tea")

    let mug = Product(code="MUG-01", name="Espresso mug", category=mugs,
                      price=7.50 EUR/pcs, cost=3.20 EUR/pcs, weight=0.2 kg)
    let beans = Product(code="BEANS-01", name="House blend", category=coffee, uom=kg,
                        price=18.00 EUR/kg, cost=9.00 EUR/kg, vat=10%)
    let sencha = Product(code="TEA-01", name="Sencha", category=tea, uom=kg,
                         price=42.00 EUR/kg, cost=20.00 EUR/kg, vat=10%)

    let roastery = Supplier(name="Roastery Bros", email="orders@roastery.example")
    SupplierOffer(product=beans, supplier=roastery, cost=9.00 EUR/kg, lead_time=3 days)

    let anna = Customer(name="Anna Rossi", email="anna@example.com",
                        address=Address(street="Via Roma 1", city="Milano", zip="20121"))
    let cafe = Customer(name="Caffè Centrale", kind=Business, vat_number="IT01234567890",
                        discount=10%, credit_limit=2_000.00 EUR, referred_by=anna)


# A fixture can extend another one: everything in `shop`, plus a confirmed order.
fixture shop_with_orders extends shop:
    let order = Order(customer=cafe, lines=[
        OrderLine(product=beans, qty=5 kg),
        OrderLine(product=mug, qty=12 pcs),
    ])
    order.confirm()
```

A test file imports the fixtures it uses:

```quadra
from tests.fixtures import shop, shop_with_orders


test "an order's life: draft, confirmed, shipped":
    use shop
    clock 2026-11-21 10:30
    let o = Order(customer=anna, lines=[OrderLine(product=mug, qty=2 pcs)])
    expect o.state is Draft
    expect o.date == 2026-11-21                     # today(), as set by the clock
    expect o.deliver_by == 2026-11-24
    expect o.number == none
    # …
```

The tests of steps 6 and 7 (`tests/orders.test.qd`, `tests/invoices.test.qd`) now `use` the
fixtures instead of creating their products and customers, and are much shorter:

```quadra
test "a confirmed order is frozen":
    use shop_with_orders
    expect order.total == 187.92 EUR
    expect_error "Order is frozen (state is not Draft): its OrderLine lines cannot change":
        order.lines[0].qty = 6 kg
    order.notes = "Deliver before 8am"              # listed in `except`

    # the world moves on, the confirmed order does not
    beans.vat = 4%
    cafe.discount = 20%
    expect order.total == 187.92 EUR
    let next = Order(customer=cafe, lines=[OrderLine(product=beans, qty=5 kg)])
    expect next.total == 74.88 EUR
```

The same fixtures can also fill a development database: `quadra db fixture shop` (step 11).

## The toolbox

| Statement | What it does |
|---|---|
| `use name` | runs a fixture (and the ones it `extends`) as one transaction |
| `clock 2026-11-21 10:30` | sets what `now()` and `today()` return from here on (a date alone means 09:00) |
| `expect cond` | fails the test if `cond` is false, showing both sides of a comparison |
| `expect_error "text":` | the indented statements must fail with an error containing `text` |
| `expect_compile_error "text":` | the indented code must **not compile**, with an error containing `text` |
| `as login:` | runs statements with the permissions of a user (step 9) |
| `patch Entity:` | inside `expect_compile_error`: replaces a definition, to test a design you rejected |
| `why(record)` | the cause chain of a record: which action, statement and user created it |

### Errors that must happen

`expect_error` checks the message, and the failed statement is rolled back, so you can check
that nothing changed:

```quadra
test "actions refuse what the flow doesn't allow":
    use shop
    let empty = Order(customer=anna)
    expect_error "An order needs at least one line":
        empty.confirm()
    expect empty.state is Draft                     # the transaction was rolled back
```

### Code that must not compile

`tests/compiler.test.qd` documents design decisions the compiler enforces:

```quadra
# Mistakes the compiler catches before any data is touched.
# `patch` (tests only) replaces a definition, to show what would happen if we wrote it that way.

test "a formula cannot depend on itself":
    expect_compile_error "formula cycle: Order.shipping → Order.total → Order.shipping":
        patch Order:
            shipping: Money<EUR> = when:
                total >= FREE_SHIPPING_FROM     => 0 EUR
                else                            => SHIPPING_FEE


test "a match on a state must handle every state":
    expect_compile_error "missing Cancelled":
        extend Order:
            label: Text = match state:
                Draft       => "Quote"
                Confirmed   => "To ship"
                Shipped     => "Done"


test "optional values must be handled":
    expect_compile_error "`o.ship_to` is optional: use `?.`":
        fn city(o: Order) -> Text:
            return o.ship_to.city
```

"Free shipping when the *total* is over 50 €" sounds reasonable — but the total includes the
shipping. The first test records why `shipping` reads `subtotal`.

`match` on an enum must cover every value. If the expected message is wrong, the test tells
you what the compiler actually said:

```text
✗ a match on a state must handle every state  (tests/compiler.test.qd:12)
    tests/compiler.test.qd:13: expected a compile error containing "missing Shipped", got: non-exhaustive match: missing Cancelled
```

### Where did this come from?

Every record knows its cause — the action, the test statement or the API request that
created it, and the user:

```quadra
test "why: every record knows where it comes from":
    use shop_with_orders
    let inv = order.create_invoice()
    expect why(inv).contains("because Order.create_invoice")
```

`why(inv)` reads like this:

```text
Invoice Invoice#37 created
  because Order.create_invoice  (system, 2026-11-02 09:00)
  because test statement at tests/orders.test.qd:86  (system, 2026-11-02 09:00)
```

## Running tests

```bash
quadra test
```

```text
✓ a formula cannot depend on itself
✓ a match on a state must handle every state
✓ optional values must be handled
✓ order totals
✓ rollups across several hops
…
✓ why: every record knows where it comes from
…
✓ the compiler catches unit and currency mistakes

28 passed, 0 failed
```

Run only the tests whose name contains a text:

```bash
quadra test --filter invoice
```

```text
✓ order → invoice → payments
✓ a posted invoice does not change
✓ a paid invoice cannot be cancelled

3 passed, 0 failed
```

Run them **on a real database**: each test gets a fresh SQLite database; at the end the
database is reloaded and verified against the engine (history, tables and formula columns):

```bash
quadra test --db sqlite --filter invoice
```

```text
database: sqlite (a fresh database per test, reloaded and verified at the end)

✓ order → invoice → payments
✓ a posted invoice does not change
✓ a paid invoice cannot be cancelled

3 passed, 0 failed
```

`--db postgres://user@host:port/db` does the same on PostgreSQL.

## Try it

1. Add a fixture `shop_with_invoice extends shop_with_orders` that ships the order, creates
   the invoice and posts it. Use it in a new test.
2. Write a test with `clock` that confirms an order on 31 December and checks that the next
   one, confirmed on 1 January, is `SO/2027/00001`.
3. Write an `expect_compile_error` for summing `l.qty` over the lines of an order.

## Recap

- Each test starts empty; each statement is a transaction.
- Fixtures share setup data, can extend each other, and also load demo databases.
- `clock` makes time deterministic; `expect_error` and `expect_compile_error` test what must
  fail; `patch` tests designs you rejected.
- `quadra test --filter` selects tests; `--db` runs them on SQLite or PostgreSQL.

[Next: permissions →](./09-permissions)
