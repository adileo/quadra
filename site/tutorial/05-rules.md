# Step 5: Rules

<SandboxLink project="tutorial-05" file="customers.qd" />

**Goal:** make bad data impossible. A business customer must have a VAT number, and only one
customer may have it; discounts have limits; an order cannot be delivered before it is
placed; a line has a positive quantity; and when an order goes over the customer's credit
limit, we want to *know* — without blocking the clerk yet.

## Rules are part of the model

A rule in Quadra is not code that someone has to remember to call. It is declared next to the
data, and the engine enforces it **at every commit**, whatever made the change: a form, the
API, an action, a test, a migration. If a rule fails, the whole transaction is rolled back.

| Rule | Example | When it fails |
|---|---|---|
| `where` on a field | `discount: Percent where 0%..30%` | the value is out of its domain |
| `required when` | `vat_number: VatNumber? required when kind is Business` | an optional field is empty when it must not be |
| `unique` | `email: Email? unique`, `unique (order, product)` | a duplicate |
| `unique (…) when` | `unique (vat_number) when kind is Business` | a duplicate, among the records where the condition holds |
| `check cond, "msg"` | `check deliver_by >= date, "…"` | the condition is false |
| `warn cond, "msg"` | `warn total <= customer.credit_limit, "…"` | never blocks: the message is attached to the record |

A `where` can be a comparison (`where > 0`), a range (`where 0%..30%`) or a lambda over the
value (`where n => n.len() >= 2`).

## The code

`customers.qd`:

```quadra{10-13,16-17,22-23}
# The people we sell to.

record Address:
    street: Text
    city: Text
    zip: Text


entity Customer:
    name: Text unique where n => n.len() >= 2
    kind: enum(Person, Business) default Person
    email: Email? unique
    vat_number: VatNumber? required when kind is Business
    address: Address?
    since: Date default today()
    discount: Percent where 0%..30% default 0%
    credit_limit: Money<EUR> where >= 0 default 500.00 EUR

    referred_by: Customer?
    referrals: [Customer] via referred_by

    unique (vat_number) when kind is Business
    check referred_by != self, "A customer cannot refer themselves"
```

In `catalog.qd`, VAT can only be one of the Italian rates, and a supplier needs at least a day:

```quadra
    vat: Percent where v => v in {4%, 10%, 22%} default 22%
```

```quadra
    lead_time: Duration where >= 1 day default 7 days
```

In `orders.qd`, a delivery date with a sensible default, a check, a warning, and rules on the
lines:

```quadra{4,19-20,26,28,33}
entity Order:
    customer: Customer
    date: Date default today()
    deliver_by: Date default date + 3 days
    ship_to: Address? default customer.address
    notes: Text?
    lines: [OrderLine]

    subtotal = lines.sum(l => l.amount)
    vat_summary = lines.group_by(l => l.vat).map(g => (
        rate: g.key,
        base: g.items.sum(l => l.amount),
        vat: (g.items.sum(l => l.amount) * g.key).round()))
    vat = vat_summary.sum(v => v.vat)
    shipping: Money<EUR> = when:
        # …
    total = subtotal + vat + shipping

    check deliver_by >= date, "The delivery date is before the order date"
    warn total <= customer.credit_limit, "This order is over the customer's credit limit"


entity OrderLine:
    order: Order parent
    product: Product
    qty: Qty<product.uom> where > 0
    price: Price<EUR, product.uom> default product.price
    discount: Percent where 0%..50% default order.customer.discount
    vat: Percent default product.vat

    amount = (qty * price * (1 - discount)).round()

    unique (order, product)                 # one line per product
```

- `deliver_by: Date default date + 3 days` — a default can use other fields of the record.
- `check` blocks the transaction. `warn` records a warning on the record: it shows up as
  `record.warnings` in tests, in the API (`"warnings": [...]`) and in the web client.
- `unique (order, product)` on a line means "each product at most once per order".

::: tip Rules hold whenever their inputs change
A `check` or a `where` is re-evaluated whenever something it reads changes — not only when
the record itself is edited. Lowering a customer's credit limit below an open order's total
immediately adds the warning to that order.
:::

## Tests

`tests/rules.test.qd`:

```quadra
test "required when and conditional unique":
    expect_error "Customer.vat_number is required":
        Customer(name="Caffè Centrale", kind=Business)
    Customer(name="Caffè Centrale", kind=Business, vat_number="IT01234567890")
    expect_error "unique (vat_number): IT01234567890 already exists":
        Customer(name="Bar Sport", kind=Business, vat_number="IT01234567890")
    # people don't need a VAT number
    Customer(name="Anna Rossi")


test "field domains":
    expect_error "Customer.name = Z: does not satisfy its `where`":
        Customer(name="Z")
    let anna = Customer(name="Anna Rossi")
    expect_error "Customer.discount = 40%: does not satisfy its `where`":
        anna.discount = 40%
    expect_error "Product.vat = 5%: does not satisfy its `where`":
        Product(code="MUG-01", name="Espresso mug", price=7.50 EUR/pcs, vat=5%)


test "checks block, warnings don't":
    let mug = Product(code="MUG-01", name="Espresso mug", price=7.50 EUR/pcs)
    let anna = Customer(name="Anna Rossi")
    expect_error "A customer cannot refer themselves":
        anna.referred_by = anna
    expect_error "The delivery date is before the order date":
        Order(customer=anna, date=2026-11-21, deliver_by=2026-11-20)

    let o = Order(customer=anna, lines=[OrderLine(product=mug, qty=100 pcs)])
    expect o.total == 915.00 EUR
    expect o.warnings == ["This order is over the customer's credit limit"]
    o.lines[0].qty = 10 pcs
    expect o.warnings == []


test "one line per product":
    let mug = Product(code="MUG-01", name="Espresso mug", price=7.50 EUR/pcs)
    let o = Order(customer=Customer(name="Anna Rossi"), lines=[OrderLine(product=mug, qty=1 pcs)])
    expect_error "unique (order, product)":
        OrderLine(order=o, product=mug, qty=2 pcs)
    expect_error "OrderLine.qty = 0 pcs: does not satisfy its `where`":
        o.lines[0].qty = 0 pcs
```

`expect_error "text":` runs the indented statements and passes if one of them fails with an
error containing `text`. The failed statement is rolled back, so the test can go on.

```bash
quadra test
```

```text
✓ order totals
✓ rollups across several hops
✓ a customer's tier follows their orders
✓ formulas must be pure
✓ categories, suppliers and tags
✓ an order with lines
✓ required when and conditional unique
✓ field domains
✓ checks block, warnings don't
✓ one line per product
✓ a new customer
✓ new products are active
✓ prices, quantities and units
✓ money is exact
✓ dates and durations
✓ the compiler catches unit and currency mistakes

16 passed, 0 failed
```

## Try it

1. Add `warn email != none, "We can't send the invoice by email"` to `Customer` and check
   `anna.warnings` in a test.
2. Allow at most 20 lines per order with a `check`.
3. A `Business` customer gets at most 30% discount, a `Person` at most 10%: write it as a
   `check` with `implies` (`kind is Person implies discount <= 10%`).

## Recap

- Rules live next to the data and are enforced at every commit, from every entry point.
- `where` constrains a field; `required when` makes an optional field mandatory under a
  condition; `unique` (also composite and conditional) prevents duplicates.
- `check` blocks and rolls back; `warn` keeps a visible, traceable warning.
- `expect_error` tests that something is refused.

[Next: flows and actions →](./06-flows-actions)
