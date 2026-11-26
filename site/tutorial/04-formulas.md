# Step 4: Formulas

<SandboxLink project="tutorial-04" file="orders.qd" />

**Goal:** compute what an order is worth — line amounts with the customer's discount, VAT at
different rates, free shipping above 50 €, the total — and roll numbers up: what each
product sold, what each category earned, which tier a customer is in.

## `=` is a formula

In Quadra, `name = expression` declares a **formula**: a value computed from other values,
**always up to date** and never written by hand — like a cell in a spreadsheet. Change a
quantity, and the line amount, the order total, the customer's lifetime value and their tier
all follow, in the same transaction.

Compare with `default`, which you met in the previous steps:

| | `amount = qty * price` | `price: Price<…> default product.price` |
|---|---|---|
| when | always | once, when the record is created |
| stored | as a materialized column, recomputed at each commit | yes, as an ordinary value |
| editable | no | yes |
| use it for | anything derived | a snapshot: "the price we agreed on" |

## Lambdas and collections

Collections have methods that take a **lambda**, `x => expression`:

```quadra
lines.sum(l => l.amount)                          # total of a list
lines.where(l => l.qty > 0)                       # filter
lines.map(l => l.product)                         # transform
lines.all(l => l.product.active)                  # any / all / count
orders.flat_map(o => o.lines)                     # a list of lists, flattened
lines.group_by(l => l.vat)                        # groups, each with `key` and `items`
offers.sort_by(o => o.cost).first()               # sort, then the first (optional)
```

Lambdas are always explicit: inside `where(l => …)` you write `l.qty`, never just `qty`, so
it's always clear whose field you're reading.

## The code

`orders.qd` gets the numbers (new lines highlighted):

```quadra{6-7,17-27,35-36,38,43-59}
# Orders: a document with lines, and the numbers we compute from them.

from customers import Customer, Address
from catalog import Product, Category

const FREE_SHIPPING_FROM: Money<EUR> = 50.00 EUR
const SHIPPING_FEE: Money<EUR> = 4.90 EUR


entity Order:
    customer: Customer
    date: Date default today()
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
        lines.count() == 0                  => 0 EUR
        subtotal >= FREE_SHIPPING_FROM      => 0 EUR
        else                                => SHIPPING_FEE
    total = subtotal + vat + shipping


entity OrderLine:
    order: Order parent
    product: Product
    qty: Qty<product.uom>
    price: Price<EUR, product.uom> default product.price
    discount: Percent default order.customer.discount
    vat: Percent default product.vat

    amount = (qty * price * (1 - discount)).round()


extend Customer:
    orders: [Order]
    lifetime_value = orders.sum(o => o.total)
    # several hops: customer → orders → lines → product → category
    categories = orders.flat_map(o => o.lines).flat_map(l => l.product.category).distinct()
    tier: enum(Standard, Silver, Gold) = when:
        lifetime_value >= 1_000.00 EUR      => Gold
        lifetime_value >= 250.00 EUR        => Silver
        else                                => Standard


extend Product:
    order_lines: [OrderLine] via product
    sold = order_lines.sum(l => l.qty)
    revenue = order_lines.sum(l => l.amount)


extend Category:
    revenue = products.sum(p => p.revenue)
```

What's going on:

- **`const`** names a value used in several places.
- **`vat_summary`** groups the lines by VAT rate and builds a list of **named tuples**
  `(rate: …, base: …, vat: …)`. VAT is computed per rate on the total of the group, then
  rounded — the way an invoice does it — not rounded line by line.
- **`when:`** picks the first condition that holds; `else` covers the rest. One condition per
  line, `condition => value`.
- **`tier: enum(Standard, Silver, Gold) = when: …`** — a formula can be an enum. The values
  are listed inline.
- **`categories`** goes through four hops. `flat_map(l => l.product.category)` also drops the
  products with no category (`none`), and `distinct()` removes duplicates.
- **`sold = order_lines.sum(l => l.qty)`** sums quantities. That compiles because every
  `l` in `order_lines` has `l.product == self`, so they are all in this product's unit. The
  compiler knows the inverse relation and uses it.

Formulas can have a type (`shipping: Money<EUR> = …`), or let the compiler infer it.

`catalog.qd` gets three formulas on `Product`:

```quadra
    label = f"{code} · {name}"
    margin = price - cost
    best_offer = offers.sort_by(o => o.cost).first()
```

`f"…{expr}…"` is a formatted string.

## What the compiler checks

**Sums across different units.** "How many items are in this order?" sounds innocent:

```quadra
    items = lines.sum(l => l.qty)
```

```text
error: cannot sum Qty<l.product.uom>: quantities in different units (each `l` has its own); convert them with .to() first
   ┌─ orders.qd:28:13
   │
28 │     items = lines.sum(l => l.qty)
   │             ^^^^^^^^^^^^^^^^^^^^^
```

Two mugs and half a kilo of coffee don't add up. (The same goes for amounts in different
currencies.)

**Cycles.** A formula cannot depend on itself:

```text
error: formula cycle: Order.balance → Order.paid → Order.balance (break it with a `default` snapshot, or make one value stored)
```

**Purity.** A formula must give the same result every time you read it, so `today()` and
`now()` are not allowed in formulas (they are fine in defaults and actions):

```quadra
test "formulas must be pure":
    expect_compile_error "today() is not allowed in a formula":
        extend Order:
            age = today() - date
```

## Tests

`tests/formulas.test.qd`:

```quadra
test "order totals":
    let mug = Product(code="MUG-01", name="Espresso mug", price=7.50 EUR/pcs)
    let beans = Product(code="BEANS-01", name="House blend", uom=kg, price=18.00 EUR/kg, vat=10%)
    let anna = Customer(name="Anna Rossi", discount=10%)
    let o = Order(customer=anna, lines=[
        OrderLine(product=mug, qty=2 pcs),
        OrderLine(product=beans, qty=500 g),
    ])
    expect o.lines.map(l => l.amount) == [13.50 EUR, 8.10 EUR]     # 10% off for Anna
    expect o.subtotal == 21.60 EUR
    expect o.vat_summary.map(v => v.vat) == [2.97 EUR, 0.81 EUR]    # 22% and 10%
    expect o.vat == 3.78 EUR
    expect o.shipping == 4.90 EUR                                   # under 50 EUR
    expect o.total == 30.28 EUR

    # formulas are always up to date: change a quantity and everything follows
    o.lines[1].qty = 3 kg
    expect o.subtotal == 62.10 EUR
    expect o.shipping == 0 EUR
    expect o.total == 69.93 EUR


test "rollups across several hops":
    let mugs = Category(name="Mugs")
    let coffee = Category(name="Coffee")
    let mug = Product(code="MUG-01", name="Espresso mug", category=mugs, price=7.50 EUR/pcs, cost=3.00 EUR/pcs)
    let beans = Product(code="BEANS-01", name="House blend", category=coffee, uom=kg, price=18.00 EUR/kg)
    let anna = Customer(name="Anna Rossi")
    Order(customer=anna, lines=[OrderLine(product=mug, qty=2 pcs)])
    Order(customer=anna, lines=[OrderLine(product=mug, qty=1 pcs), OrderLine(product=beans, qty=1 kg)])

    expect mug.label == "MUG-01 · Espresso mug"
    expect mug.margin == 4.50 EUR/pcs
    expect mug.sold == 3 pcs
    expect mug.revenue == 22.50 EUR
    expect mugs.revenue == 22.50 EUR
    expect anna.categories == [mugs, coffee]
    expect anna.tier is Standard


test "a customer's tier follows their orders":
    let beans = Product(code="BEANS-01", name="House blend", uom=kg, price=18.00 EUR/kg, vat=10%)
    let anna = Customer(name="Anna Rossi")
    let o = Order(customer=anna, lines=[OrderLine(product=beans, qty=10 kg)])
    expect anna.lifetime_value == 198.00 EUR
    expect anna.tier is Standard
    o.lines[0].qty = 20 kg
    expect anna.tier is Silver
```

`x is Variant` tests an enum value.

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
✓ a new customer
✓ new products are active
✓ prices, quantities and units
✓ money is exact
✓ dates and durations
✓ the compiler catches unit and currency mistakes

12 passed, 0 failed
```

## Where does a number come from?

`quadra trace` shows what a value depends on, down to the stored fields, and who writes them:

```bash
quadra trace Order.total
```

```text
Order.total : (inferred)    formula · orders.qd:27
  = (subtotal + vat) + shipping

Depends on
  Order.shipping  = when: lines.count() == 0 => 0 EUR; subtotal >= FREE_SHIPPING_FROM => 0 EUR; else => SHIPPING_FEE
    Order.lines  stored
    Order.subtotal  = lines.sum(l => l.amount)
      OrderLine.amount  = ((qty * price) * (1 - discount)).round()
        OrderLine.discount  stored · default order.customer.discount
        OrderLine.price  stored · default product.price
        OrderLine.qty  stored
  Order.subtotal  (see above)
  Order.vat  = vat_summary.sum(v => v.vat)
    Order.vat_summary  = lines.group_by(l => l.vat).map(g => (rate: g.key, base: g.items.sum(l => l.amount), vat: (g.items.sum(l => l.amount) * g.key).round()))
      OrderLine.amount  (see above)
      OrderLine.vat  stored · default product.vat

Stored values underneath, and who writes them
  Order.lines
      user, through forms and API
  OrderLine.discount
      user, through forms and API
  OrderLine.price
      user, through forms and API
  OrderLine.qty
      user, through forms and API
  OrderLine.vat
      user, through forms and API

Read by
  Customer.lifetime_value
```

It's the answer to "why is this total what it is?", for you and for an auditor.

## Try it

1. Add `items_count = lines.count()` to `Order`, and
   `average_line: Money<EUR> = subtotal / items_count`. Read the error, and fix it.
2. Add `best_customers` to `Category`: the distinct customers who bought its products
   (hint: `products.flat_map(p => p.order_lines).map(l => l.order.customer).distinct()`).
3. Try to make shipping free for `Gold` customers by adding `customer.tier is Gold => 0 EUR`
   to `shipping`. The compiler finds a cycle: `Customer.lifetime_value → Order.total →
   Order.shipping → Customer.tier → Customer.lifetime_value`. Why is it right? (A way out:
   decide the shipping fee once, with a `default`, instead of a formula.)

## Recap

- `name = expr` is a formula: always current, read-only. `default` is a snapshot.
- Collections take explicit lambdas: `sum`, `where`, `map`, `flat_map`, `group_by`, `count`,
  `all`, `any`, `distinct`, `sort_by`, `first`, `single`.
- `when:` chooses a value; enums can be declared inline.
- Rollups can cross as many relations as you need; the compiler still checks units and
  currencies, cycles and purity.
- `quadra trace Entity.field` explains any value.

[Next: rules →](./05-rules)
