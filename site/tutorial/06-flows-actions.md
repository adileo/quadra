# Step 6: Flows and actions

<SandboxLink project="tutorial-06" file="orders.qd" />

**Goal:** give orders a life. An order starts as a draft that the clerk can edit freely. When
the customer confirms, it gets a number, the credit limit is enforced, and it **stops
changing**. Then it is shipped — or cancelled, with a reason.

## States and flows

A state is an ordinary enum field. A `flow` declares which transitions exist and which
**action** performs each one:

```quadra
    state: enum(Draft, Confirmed, Shipped, Cancelled) default Draft

    flow state:
        Draft -> Confirmed                  by confirm
        Confirmed -> Shipped                by ship
        Draft | Confirmed -> Cancelled      by cancel
```

Those are the only ways to move an order along: the state cannot be assigned
(`o.state = Shipped` is a compile error that names the actions to use, and the API refuses
it too). Running `confirm` on an order that is already confirmed fails with
`Order.confirm is not allowed in state Confirmed`. Transitions
can also have guards (`by cancel unless paid > 0 EUR`, which you'll use in the next step) or
happen automatically (`Confirmed -> Done when …`, without `by`).

## Actions

An `action` is an operation on a record that runs as **one transaction**: everything it does
is committed together, or — if anything fails — nothing is.

```quadra
    action confirm:
        require lines.count() > 0, "An order needs at least one line"
        require lines.all(l => l.product.active), "The order contains products we no longer sell"
        number = next_number(current.company, "SO")
        confirmed_at = now()
```

- `require cond, "message"` stops the action (and rolls everything back) when `cond` is false.
- The action body assigns fields; when it completes, the flow moves the state to `Confirmed`.
- `next_number(company, "SO")` takes the next number of a sequence: `SO/2026/00001`,
  `SO/2026/00002`… It is atomic, so two clerks never get the same number.
- `now()` and `today()` are allowed in actions and defaults, not in formulas.

Why a `require` in `confirm`, and not `product: Product where p => p.active` on the line? A
`where` holds on every draft at all times: a product could not be deactivated while some
draft still contains it. The `require` lets a draft keep it and stops only its confirmation.
(Confirmed and shipped orders never stand in the way: the rules of a frozen record held when
it was frozen, and a `where` is not checked again against later data.)

Actions can take parameters, and can send messages to the outside world with `emit`:

```quadra
# A message to the outside world: it leaves only after the transaction commits.
event ShippingNotice(to: Email, order: Order)
```

```quadra
    action ship:
        if let email = customer.email:
            emit ShippingNotice(to=email, order=self)

    action cancel(reason: Text):
        cancel_reason = reason
```

An `event` is a typed message for integrations (e-mail, a carrier, a webhook). Emitted events
go to an outbox and leave **after** the commit: a rolled-back transaction never sends an email.
With a database, the outbox is a table written together with the commit; the program that
actually sends the emails reads it with [`quadra outbox`](/reference/cli#events-quadra-outbox).

## Frozen documents

Once confirmed, an order is a fact. `frozen when` says from when:

```quadra
    # Once confirmed, the order is a fact: it sees the world as it was at that moment.
    frozen when state is not Draft except notes, cancel_reason
```

A frozen record:

1. **cannot be edited** — neither its fields nor its lines (they are its `parent` lines) —
   except for the fields listed in `except`;
2. **sees the world as it was when it was frozen**: its formulas — and those of its lines —
   keep reading the customer's discount, the product's VAT rate and so on as they were at
   confirmation. Next year's VAT change does not rewrite this year's orders.

It stays frozen while the condition holds. With a transition back to `Draft` (say
`Confirmed -> Draft by reopen`), reopening the order makes it and its lines editable again,
and its formulas follow the data from then on.

## The credit limit, now for real

In the previous step the credit limit was a warning on the order. Now that orders have states,
we can say precisely what it means: the confirmed, not yet shipped orders of a customer must
stay within the limit.

```quadra{3-4,11}
extend Customer:
    orders: [Order]
    lifetime_value = orders.where(o => o.state in {Confirmed, Shipped}).sum(o => o.total)
    open_total = orders.where(o => o.state is Confirmed).sum(o => o.total)
    categories = orders.flat_map(o => o.lines).flat_map(l => l.product.category).distinct()
    tier: enum(Standard, Silver, Gold) = when:
        lifetime_value >= 1_000.00 EUR      => Gold
        lifetime_value >= 250.00 EUR        => Silver
        else                                => Standard

    check open_total <= credit_limit, "Credit limit exceeded"
```

The `check` is on the customer, but it is evaluated whenever `open_total` changes — so
confirming an order that would break it fails, and the confirmation is rolled back. A draft
stays a draft with its warning; confirming it is refused.

## The whole order

```quadra{14,22-24,41-60}
# Orders: a document with lines, a life cycle, and the numbers we compute from them.

from customers import Customer, Address
from catalog import Product, Category

const FREE_SHIPPING_FROM: Money<EUR> = 50.00 EUR
const SHIPPING_FEE: Money<EUR> = 4.90 EUR

# A message to the outside world: it leaves only after the transaction commits.
event ShippingNotice(to: Email, order: Order)


entity Order:
    number: Code? unique
    customer: Customer
    date: Date default today()
    deliver_by: Date default date + 3 days
    ship_to: Address? default customer.address
    notes: Text?
    lines: [OrderLine]

    state: enum(Draft, Confirmed, Shipped, Cancelled) default Draft
    confirmed_at: DateTime? required when state in {Confirmed, Shipped}
    cancel_reason: Text? required when state is Cancelled

    subtotal = lines.sum(l => l.amount)
    vat_summary = lines.group_by(l => l.vat).map(g => (
        rate: g.key,
        base: g.items.sum(l => l.amount),
        vat: (g.items.sum(l => l.amount) * g.key).round()))
    vat = vat_summary.sum(v => v.vat)
    shipping: Money<EUR> = when:
        lines.count() == 0                  => 0 EUR
        subtotal >= FREE_SHIPPING_FROM      => 0 EUR
        else                                => SHIPPING_FEE
    total = subtotal + vat + shipping

    check deliver_by >= date, "The delivery date is before the order date"
    warn total <= customer.credit_limit, "This order is over the customer's credit limit"

    # Once confirmed, the order is a fact: it sees the world as it was at that moment.
    frozen when state is not Draft except notes, cancel_reason

    flow state:
        Draft -> Confirmed                  by confirm
        Confirmed -> Shipped                by ship
        Draft | Confirmed -> Cancelled      by cancel

    action confirm:
        require lines.count() > 0, "An order needs at least one line"
        require lines.all(l => l.product.active), "The order contains products we no longer sell"
        number = next_number(current.company, "SO")
        confirmed_at = now()

    action ship:
        if let email = customer.email:
            emit ShippingNotice(to=email, order=self)

    action cancel(reason: Text):
        cancel_reason = reason
```

`confirmed_at: DateTime? required when state in {Confirmed, Shipped}` ties a field to the
state: a confirmed order without a confirmation time cannot exist.

## Tests

`tests/orders.test.qd`. `clock` sets what `now()` and `today()` return, so tests are
repeatable:

```quadra
test "an order's life: draft, confirmed, shipped":
    clock 2026-11-21 10:30
    let mug = Product(code="MUG-01", name="Espresso mug", price=7.50 EUR/pcs)
    let anna = Customer(name="Anna Rossi", email="anna@example.com")
    let o = Order(customer=anna, lines=[OrderLine(product=mug, qty=2 pcs)])
    expect o.state is Draft
    expect o.number == none

    o.confirm()
    expect o.state is Confirmed
    expect o.number == "SO/2026/00001"
    expect o.confirmed_at == 2026-11-21 10:30
    expect anna.open_total == 23.20 EUR

    o.ship()
    expect o.state is Shipped
    expect anna.open_total == 0 EUR
    expect anna.lifetime_value == 23.20 EUR


test "actions refuse what the flow doesn't allow":
    let mug = Product(code="MUG-01", name="Espresso mug", price=7.50 EUR/pcs)
    let anna = Customer(name="Anna Rossi")
    let empty = Order(customer=anna)
    expect_error "An order needs at least one line":
        empty.confirm()
    expect empty.state is Draft                     # the transaction was rolled back

    let o = Order(customer=anna, lines=[OrderLine(product=mug, qty=2 pcs)])
    o.confirm()
    expect_error "Order.confirm is not allowed in state Confirmed":
        o.confirm()
    o.ship()
    expect_error "Order.cancel is not allowed in state Shipped":
        o.cancel(reason="Changed my mind")


test "the credit limit is enforced at confirmation":
    let mug = Product(code="MUG-01", name="Espresso mug", price=7.50 EUR/pcs)
    let anna = Customer(name="Anna Rossi")
    let big = Order(customer=anna, lines=[OrderLine(product=mug, qty=100 pcs)])
    expect big.warnings == ["This order is over the customer's credit limit"]
    expect_error "Credit limit exceeded":
        big.confirm()
    expect big.state is Draft
    big.cancel(reason="Over the credit limit")
    expect big.state is Cancelled


test "a confirmed order is frozen":
    let mug = Product(code="MUG-01", name="Espresso mug", price=7.50 EUR/pcs)
    let anna = Customer(name="Anna Rossi")
    let o = Order(customer=anna, lines=[OrderLine(product=mug, qty=2 pcs)])
    o.confirm()
    expect_error "Order is frozen (state is not Draft): its OrderLine lines cannot change":
        o.lines[0].qty = 3 pcs
    o.notes = "Gift wrap, please"                   # listed in `except`

    # the world moves on, the confirmed order does not
    mug.vat = 10%
    anna.discount = 10%
    expect o.total == 23.20 EUR
    let next = Order(customer=anna, lines=[OrderLine(product=mug, qty=2 pcs)])
    expect next.total == 19.75 EUR
```

The file also tests that inactive products can't be confirmed, and cancelling a confirmed
order. Because the tier now counts only confirmed and shipped orders, the tier test in
`tests/formulas.test.qd` confirms its orders before checking the tier.

```bash
quadra test
```

```text
…
✓ an order's life: draft, confirmed, shipped
✓ actions refuse what the flow doesn't allow
✓ the credit limit is enforced at confirmation
✓ a confirmed order is frozen
✓ only active products can be confirmed
✓ cancelling a confirmed order
…

22 passed, 0 failed
```

## Try it

1. Add an `OnHold` state with `Confirmed -> OnHold by hold` and `OnHold -> Confirmed by release`.
   Should an order on hold count in `open_total`? Update the formula and test it.
2. Add `shipped_at: DateTime? required when state is Shipped` and set it in `ship`.
3. Make cancellation of a confirmed order require a reason of at least 10 characters
   (`require reason.len() >= 10, "…"`).

## Recap

- A `flow` lists the transitions of a state field and the action that performs each one.
- An `action` is one transaction; `require` stops it and rolls everything back.
- `emit` sends typed events after the commit.
- `frozen when … except …` makes a document read-only and makes its formulas see the world
  as it was when it was frozen.
- `clock` makes time-dependent tests repeatable.

[Next: invoices — frozen and live →](./07-invoices-frozen-live)
