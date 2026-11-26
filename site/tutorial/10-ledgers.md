# Step 10: Ledgers

<SandboxLink project="tutorial-10" file="stock.qd" />

**Goal:** know how much stock we have, and how much each customer owes us — numbers that
accountants and warehouse staff can trust, because nobody types them in.

## Balances are not fields

The classic mistake in business software is a `stock` column that the code increments and
decrements. Sooner or later someone forgets an update, or two updates race, and the number
drifts away from reality — with no way to tell why.

In Quadra, a balance is the **sum of movements** in a **ledger**: an append-only register
whose rows are written only by the documents that cause them. A goods receipt adds stock; a
shipment removes it; an invoice adds to what the customer owes; a payment subtracts.

```quadra
ledger Stock:
    key product: Product
    measure qty: Qty<product.uom>

    check per (product): g => g.sum(r => r.qty) >= 0, "Not enough stock"
```

- `key` fields identify what a row is about; `measure` fields are what adds up (`attr` adds a
  descriptive field that is not a key).
- `check per (product): g => …` is a rule on each group of rows with the same `product`: the
  balance of a product can never go negative.

A document writes to a ledger with `post to`:

```quadra
    post to Stock when state is Received:
        add(product=product, qty=qty)
```

The magic is in `when`: **while the condition holds, the rows exist; when it stops holding,
they are reversed automatically** — with new rows, because a ledger is never rewritten. Cancel
a posted invoice, and its receivable disappears from the balance, with a full trace of both
movements.

## Stock

A new module, `stock.qd`, with the ledger, the receipts of goods from suppliers, and the
quantity on hand:

```quadra
# Stock is not a field that someone updates: it is the sum of what came in and went out.

from catalog import Product, Supplier


# An append-only register. Rows are written only by `post to Stock`.
ledger Stock:
    key product: Product
    measure qty: Qty<product.uom>

    check per (product): g => g.sum(r => r.qty) >= 0, "Not enough stock"


# Goods arriving from a supplier.
entity Restock:
    supplier: Supplier?
    date: Date default today()
    product: Product
    qty: Qty<product.uom> where > 0
    state: enum(Draft, Received) default Draft

    frozen when state is Received
    flow state:
        Draft -> Received by receive

    post to Stock when state is Received:
        add(product=product, qty=qty)


extend Product:
    on_hand: Qty<uom> = Stock.where(s => s.product == self).sum(s => s.qty)
```

In `orders.qd`, shipping an order takes the goods out of stock, and products know what is
reserved by confirmed orders:

```quadra{5,12-15}
# Orders: a document with lines, a life cycle, and the numbers we compute from them.

from customers import Customer, Address
from catalog import Product, Category
from stock import Stock

# …

    action cancel(reason: Text):
        cancel_reason = reason

    # Shipping takes the goods out of stock.
    post to Stock when state is Shipped:
        for l in lines:
            add(product=l.product, qty=-l.qty)
```

```quadra{5-6}
extend Product:
    order_lines: [OrderLine] via product
    sold = order_lines.sum(l => l.qty)
    revenue = order_lines.sum(l => l.amount)
    reserved: Qty<uom> = order_lines.where(l => l.order.state is Confirmed).sum(l => l.qty)
    available = on_hand - reserved
```

A `post to` body can loop and call `add(…)` as many times as needed: one row per line here.

## Receivables

`receivables.qd` declares what customers owe us:

```quadra
# What each customer owes us. Invoices and payments post here (see invoices.qd).

from customers import Customer


ledger Receivables:
    key customer: Customer
    measure amount: Money<EUR>


extend Customer:
    balance: Money<EUR> = Receivables.where(r => r.customer == self).sum(r => r.amount)
```

and `invoices.qd` posts to it from invoices and payments (it imports
`from receivables import Receivables`):

```quadra
    # If the invoice leaves Posted (it is cancelled), its rows are reversed automatically.
    post to Receivables when state is Posted:
        add(customer=customer, amount=total)
```

```quadra{7-8}
entity Payment:
    invoice: Invoice
    date: Date default today()
    amount: Money<EUR> where > 0
    method: Text default "card"             # free text for now: see the migrations step

    post to Receivables:
        add(customer=invoice.customer, amount=-amount)
```

A `post to` without `when` posts as long as the record exists.

::: tip `post to` in an `extend`
Here `post to` sits inside the `entity` block, but it works in an `extend` block too: a
module that adds a ledger can make existing entities post to it without touching their
declaration (`extend Order: post to Stock when state is Shipped: …`).
:::

## Permissions

The warehouse receives goods (`security.qd`):

```quadra{4-5}
role Warehouse is Staff:
    read Order
    run Order.ship
    read, create Restock
    run Restock.receive
```

## Tests

The `shop` fixture now receives some stock:

```quadra
    Restock(product=mug, qty=50 pcs).receive()
    Restock(product=beans, supplier=roastery, qty=20 kg).receive()
    Restock(product=sencha, qty=5 kg).receive()
```

`tests/ledgers.test.qd`:

```quadra
from tests.fixtures import shop, shop_with_orders


test "stock moves with restocks and shipments":
    use shop_with_orders
    expect beans.on_hand == 20 kg
    expect beans.reserved == 5 kg                   # confirmed, not shipped yet
    expect beans.available == 15 kg

    order.ship()
    expect beans.on_hand == 15 kg
    expect beans.reserved == 0 kg
    expect mug.on_hand == 38 pcs

    as wes: Restock(product=beans, supplier=roastery, qty=10 kg).receive()
    expect beans.on_hand == 25 kg
    expect Stock.where(s => s.product == beans).count() == 3


test "the ledger refuses to go below zero":
    use shop
    let o = Order(customer=cafe, lines=[OrderLine(product=sencha, qty=8 kg)])
    o.confirm()
    expect sencha.available == -3 kg                # we can promise it…
    expect_error "Not enough stock":
        o.ship()                                    # …but not ship what we don't have
    expect o.state is Confirmed
    expect sencha.on_hand == 5 kg


test "receivables: invoices add, payments subtract":
    use shop_with_orders
    let inv = order.create_invoice()
    inv.post()
    expect cafe.balance == 187.92 EUR
    Payment(invoice=inv, amount=100.00 EUR, method="transfer")
    expect cafe.balance == 87.92 EUR


test "cancelling an invoice reverses its rows":
    use shop_with_orders
    let inv = order.create_invoice()
    inv.post()
    expect cafe.balance == 187.92 EUR
    inv.cancel()
    expect cafe.balance == 0 EUR
    expect Receivables.where(r => r.customer == cafe).count() == 2     # the row and its reversal
```

The ledger's `check per (product)` is what refuses the shipment of 8 kg of sencha when we have
5: the whole transaction — the state change *and* the postings — is rolled back.

```bash
quadra test
```

```text
…
✓ stock moves with restocks and shipments
✓ the ledger refuses to go below zero
✓ receivables: invoices add, payments subtract
✓ cancelling an invoice reverses its rows
…

36 passed, 0 failed
```

## Try it

1. Add `key reason: enum(Sale, Purchase, Adjustment)` to `Stock` and fill it in each `add`.
   Then write `sold_from_stock` on `Product`: the sum of the `Sale` rows.
2. Add an `Adjustment` entity (a stock count correction) that posts a positive or negative
   quantity when it is approved.
3. Add `stock_value: Money<EUR> = (on_hand * cost).round()` to `Product`, and a
   `Category.stock_value` that adds up its products.

## Recap

- A ledger is an append-only register: `key` fields, `measure` fields that add up, and
  `check per (…)` rules on groups.
- Documents write rows with `post to Ledger when …: add(…)`; when the condition stops
  holding, the rows are reversed automatically.
- Balances are formulas over the ledger, so they cannot drift.

[Next: the database and the API →](./11-database-and-api)
