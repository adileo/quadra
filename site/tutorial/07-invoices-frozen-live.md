# Step 7: Invoices — frozen and live

<SandboxLink project="tutorial-07" file="invoices.qd" />

**Goal:** bill confirmed orders. An order may be invoiced in one go or in parts; an invoice is
posted, then paid in one or more payments. Posted invoices never change — but they must still
tell us how much is left to pay. And a confirmed order, which is frozen, must still tell us
whether it has been invoiced.

This is the step where documents start to **follow other documents**, and where `frozen` meets
its counterpart: `live`.

## The invoice

A new module, `invoices.qd`:

```quadra
# Invoices and payments: documents that arrive after the order is confirmed.

from customers import Customer
from catalog import Product
from orders import Order, OrderLine


entity Invoice:
    number: Code? unique
    customer: Customer
    date: Date default today()
    lines: [InvoiceLine]
    shipping: Money<EUR> default 0 EUR
    payments: [Payment]
    state: enum(Draft, Posted, Cancelled) default Draft

    subtotal = lines.sum(l => l.amount)
    vat = lines.group_by(l => l.vat).sum(g => (g.items.sum(l => l.amount) * g.key).round())
    total = subtotal + vat + shipping
    paid = payments.sum(p => p.amount)
    due = total - paid
    payment_status: enum(Unpaid, Partial, Paid) = when:
        paid == 0 EUR       => Unpaid
        due > 0 EUR         => Partial
        else                => Paid

    check due >= 0 EUR, "Paid more than the invoice total"

    # A posted invoice never changes, except for what happens to it later: its payments.
    frozen when state is not Draft live paid, due, payment_status

    flow state:
        Draft -> Posted                 by post
        Draft | Posted -> Cancelled     by cancel unless paid > 0 EUR

    action post:
        require lines.count() > 0, "An invoice needs at least one line"
        number = next_number(current.company, "INV")


entity InvoiceLine:
    invoice: Invoice parent
    order_line: OrderLine?                  # where it comes from, if anywhere
    product: Product
    qty: Qty<product.uom> where > 0
    price: Price<EUR, product.uom> default product.price
    discount: Percent where 0%..100% default 0%
    vat: Percent default product.vat

    amount = (qty * price * (1 - discount)).round()


entity Payment:
    invoice: Invoice
    date: Date default today()
    amount: Money<EUR> where > 0
    method: Text default "card"             # free text for now: see the migrations step
```

Two new things in the flow and the freeze:

- `by cancel unless paid > 0 EUR` — a **guard**: a paid invoice cannot be cancelled. The error
  tells you why: `Invoice.cancel is not allowed: paid > 0 EUR`.
- `frozen when state is not Draft live paid, due, payment_status`.

## `live`: formulas that keep following the data

A frozen record sees the world as it was when it was frozen. For a posted invoice that's
exactly right for the lines, the prices and the VAT. But **payments arrive after posting**: if
`paid` were frozen, it would stay at zero forever.

`live paid, due, payment_status` lists the formulas that keep following the data. Everything
else stays as it was at posting.

`payments: [Payment]` is the inverse of `Payment.invoice`. Creating a payment for a posted
invoice is allowed: it doesn't modify the invoice, it *points to* it.

## Following the order

On the order side, each line knows how much of it has been invoiced, and the order knows its
invoices and its invoicing status:

```quadra
extend OrderLine:
    # The lines are frozen with their order, the invoices come later: these follow the data.
    live invoiced, to_invoice
    invoice_lines: [InvoiceLine] via order_line
    invoiced = invoice_lines.where(i => i.invoice.state is Posted).sum(i => i.qty.to(product.uom))
    to_invoice = qty - invoiced


extend Order:
    # The order is frozen when confirmed, the invoices come later: these follow the data.
    live invoices, invoice_status
    invoices = lines.flat_map(l => l.invoice_lines).map(i => i.invoice).distinct()
    invoice_status: enum(NotInvoiced, ToInvoice, Invoiced) = when:
        state not in {Confirmed, Shipped}       => NotInvoiced
        lines.all(l => l.to_invoice <= 0)       => Invoiced
        else                                    => ToInvoice

    action create_invoice -> Invoice:
        require state in {Confirmed, Shipped}, "Only confirmed orders can be invoiced"
        let todo = lines.where(l => l.to_invoice > 0)
        require todo.any(), "Nothing left to invoice"
        return Invoice(customer=customer,
            shipping=shipping if invoices.count() == 0 else 0 EUR,
            lines=todo.map(l => InvoiceLine(order_line=l, product=l.product, qty=l.to_invoice,
                price=l.price, discount=l.discount, vat=l.vat)))
```

- `.sum(i => i.qty.to(product.uom))` — each invoice line's quantity is in *its* product's
  unit; converting to this line's unit tells the compiler they can be added.
- `live` can be declared in an `extend`, next to the formulas it concerns: the order is
  declared in `orders.qd`, which knows nothing about invoices.
- The lines of a frozen document are frozen with it: their formulas also read the world as
  it was at confirmation. `invoiced` and `to_invoice` must follow the invoices, so the line
  declares them `live` too; `invoice_status` reads `to_invoice`, which is why both lists are
  needed.
- `action create_invoice -> Invoice` returns the record it creates. It is a normal action:
  it runs on a frozen order because it doesn't change the order.
- `a if cond else b` is the inline conditional.

### The compiler tells you when you need `live`

Remove the `live invoices, invoice_status` line and run `quadra check`:

```text
warning: `invoices` reads InvoiceLine (via order_line): records that point to the Order and come after it is frozen. On a frozen Order it would keep the value it had when frozen; if it should follow them, declare `live invoices` in this `extend`
   ┌─ invoices.qd:70:5
   │
70 │     invoices = lines.flat_map(l => l.invoice_lines).map(i => i.invoice).distinct()
   │     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

warning: `invoice_status` reads InvoiceLine (via order_line): records that point to the Order and come after it is frozen. On a frozen Order it would keep the value it had when frozen; if it should follow them, declare `live invoice_status` in this `extend`
   ┌─ invoices.qd:71:5
   │
71 │     invoice_status: enum(NotInvoiced, ToInvoice, Invoiced) = when:
   │     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

✓ 14 modules checked, 2 warnings
```

It's a warning, not an error, because sometimes a snapshot is what you want. Here it isn't:
without `live`, a confirmed order would say `ToInvoice` forever. Remove `live invoiced,
to_invoice` from the line instead, and the warning says the same of the line:

```text
warning: `invoiced` reads InvoiceLine (via order_line): records that point to the OrderLine and come after its Order is frozen. On a line of a frozen Order it would keep the value it had when frozen; if it should follow them, declare `live invoiced` in this `extend`
```

## A rollup over four hops

How many posted invoices does a customer have? Walk from the customer to its orders, their
lines, the invoice lines that come from them, and their invoices:

```quadra
extend Customer:
    # four hops: customer → orders → lines → invoice lines → invoices
    posted_invoices = orders
        .flat_map(o => o.lines)
        .flat_map(l => l.invoice_lines)
        .map(i => i.invoice)
        .distinct()
        .where(i => i.state is Posted)
        .count()
```

A line that starts with `.` continues the previous one, so long chains read top to bottom.

## Tests

`tests/invoices.test.qd`:

```quadra
test "order → invoice → payments":
    clock 2026-11-21 10:00
    let mug = Product(code="MUG-01", name="Espresso mug", price=7.50 EUR/pcs)
    let anna = Customer(name="Anna Rossi")
    let o = Order(customer=anna, lines=[OrderLine(product=mug, qty=2 pcs)])
    o.confirm()
    expect o.invoice_status is ToInvoice

    let inv = o.create_invoice()
    expect inv.total == 23.20 EUR                   # same lines, same shipping
    inv.post()
    expect inv.number == "INV/2026/00001"

    # the order is frozen, but `live` formulas follow the new invoice
    expect o.invoices == [inv]
    expect o.invoice_status is Invoiced
    expect anna.posted_invoices == 1

    Payment(invoice=inv, amount=20.00 EUR)
    expect inv.payment_status is Partial
    expect inv.due == 3.20 EUR
    expect_error "Paid more than the invoice total":
        Payment(invoice=inv, amount=5.00 EUR)
    Payment(invoice=inv, amount=3.20 EUR, method="cash")
    expect inv.payment_status is Paid


test "partial invoicing":
    let beans = Product(code="BEANS-01", name="House blend", uom=kg, price=18.00 EUR/kg, vat=10%)
    let o = Order(customer=Customer(name="Anna Rossi"), lines=[OrderLine(product=beans, qty=2 kg)])
    o.confirm()
    let first = o.create_invoice()
    first.lines[0].qty = 500 g                      # the first invoice covers half a kilo
    first.post()
    expect o.lines[0].to_invoice == 1.5 kg
    expect o.invoice_status is ToInvoice
    let second = o.create_invoice()
    expect second.lines[0].qty == 1.5 kg
    expect second.shipping == 0 EUR                 # shipping goes on the first invoice only
    second.post()
    expect o.invoice_status is Invoiced
    expect o.invoices == [first, second]
```

The file also checks that a posted invoice refuses changes
(`Invoice is frozen (state is not Draft): its InvoiceLine lines cannot change`), that an order
fully invoiced refuses a new invoice (`Nothing left to invoice`), and that a paid invoice
cannot be cancelled.

```bash
quadra test
```

```text
…
✓ order → invoice → payments
✓ a posted invoice does not change
✓ a paid invoice cannot be cancelled
✓ partial invoicing
…

26 passed, 0 failed
```

## Try it

1. Add `overdue = state is Posted and due > 0 EUR and …` — what's missing? (Hint: an invoice
   needs a due date, `due_date: Date default date + 30 days`, and "overdue" depends on today,
   so it can't be a formula: write it as a `fn overdue(on: Date) -> Bool`.)
2. Add `unpaid_total` to `Customer`: the sum of `due` over the posted invoices, using the same
   four hops as `posted_invoices`.
3. Remove `due` from the `live` list of `Invoice`. What does `quadra check` say? Which
   expectation fails in `quadra test`, and why?

## Recap

- Documents can create and follow other documents: invoices from order lines, payments for
  invoices.
- A guard (`unless …`) forbids a transition under a condition.
- `frozen when … live a, b` keeps some formulas following the data after the freeze; `live`
  can also be declared in an `extend`.
- The compiler warns when a formula of a frozen document reads records that will arrive later.
- Rollups can follow as many hops as you need with `flat_map`, `map`, `distinct` and `where`.

[Next: tests →](./08-tests)
