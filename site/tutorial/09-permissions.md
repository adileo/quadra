# Step 9: Permissions

<SandboxLink project="tutorial-09" file="security.qd" />

**Goal:** the shop has staff now. Clerks take orders — their own — but must not touch prices,
see our costs, or raise a customer's credit limit. The warehouse ships. The accountant
invoices and handles payments. The owner can do everything. And nobody cancels an invoice
they issued themselves.

## Roles and rules

Permissions are declared in the model, as **roles**. A role is a list of rules:

```text
[deny] verbs  targets  [where row-condition]
```

| Verb | Meaning |
|---|---|
| `read` | see records and fields |
| `create` | create records |
| `edit` | change records, or only some fields (`edit Customer.email`) |
| `run` | run actions (`run Order.confirm`) |
| `all` | all of the above |
| `hide` / `show` | hide fields this role could otherwise read (and show them again in a role that extends it: a field stays hidden only if no role of the user shows it) |
| `read history` | see past values and their causes |

Targets are entities, fields (`Customer.credit_limit`), actions (`Order.ship`) or `*`.
A `where` limits a rule to some rows: `where o => o.created_by == me`, where `me` is the
signed-in user.

Three principles:

1. **Nothing is allowed unless a rule allows it.**
2. **Lines follow their document**: reading or editing an `OrderLine` (a `parent` line) is
   reading or editing its `Order`.
3. **`deny` wins.** A `deny` at the top level applies to everybody, whatever roles they
   have — the owner included: it is for separation of duties. A `deny` inside a role applies
   to that role's users, even when another of their roles allows the same thing.

The engine applies the rules on every request, to every row and field: the API, the web
client, actions, and the reactions they trigger.

## The code

A new module, `security.qd`:

```quadra
# Who can do what. Without a rule that allows it, nothing is visible or editable.
# `me` is the signed-in user; `where` filters the rows a rule applies to.

from customers import Customer
from catalog import Product, Category, Tag, Supplier, SupplierOffer
from orders import Order
from invoices import Invoice, Payment


role Owner:
    all *                                   # everything (but `deny` rules still apply)


role Staff:
    read Product, Category, Tag, Uom, UomCategory, Currency


role Clerk is Staff:
    read, create Customer
    edit Customer.name, Customer.email, Customer.address
    read, create, edit Order where o => o.created_by == me
    run Order.confirm, Order.cancel where o => o.created_by == me
    hide Product.cost, Product.margin


role Warehouse is Staff:
    read Order
    run Order.ship


role Accountant is Staff:
    read Customer, Order, Supplier, SupplierOffer
    edit Customer.credit_limit
    all Invoice, Payment
    run Order.create_invoice, Invoice.post, Invoice.cancel


# Separation of duties: holds for everybody, whatever roles they have.
deny run Invoice.cancel where i => i.created_by == me
```

- `role Clerk is Staff` — a role can extend another one and inherit its rules.
- `Uom, UomCategory, Currency` are entities of the standard library: staff need to read them
  to see quantities and prices.
- `edit Customer.name, Customer.email, Customer.address` — field-level: a clerk can fix an
  email but not the credit limit, which only the accountant edits.
- `hide Product.cost, Product.margin` — clerks see products, but our costs and margins come
  back as `{"hidden": true}` in the API and are not shown in the web client.

## Users

Users are records of the standard library's `User` entity, with a login and a set of roles.
For tests, the `shop` fixture creates the staff (highlighted):

```quadra{4-8}
fixture shop:
    clock 2026-11-02 09:00

    User(login="olivia", name="Olivia", roles={Owner})
    User(login="sam", name="Sam", roles={Clerk})
    User(login="kim", name="Kim", roles={Clerk})
    User(login="wes", name="Wes", roles={Warehouse})
    User(login="alex", name="Alex", roles={Accountant})

    let mugs = Category(name="Mugs")
    # …
```

## Tests with `as`

Without `as`, a test runs as the system and permission rules are skipped. `as login:` runs a
statement as that user, with their permissions; `let x = as login: expr` keeps the result.

```quadra
# Without `as`, a test runs as the system and every rule is skipped.
# `as login:` runs a statement with the permissions of that user.

from tests.fixtures import shop


test "clerks sell, warehouse ships, accountants invoice":
    use shop
    let o = as sam: Order(customer=anna, lines=[OrderLine(product=mug, qty=2 pcs)])
    as sam: o.confirm()
    expect_error "permission denied: wes cannot create Order":
        as wes: Order(customer=anna)
    as wes: o.ship()
    let inv = as alex: o.create_invoice()
    as alex: inv.post()
    expect why(inv).contains("alex")


test "row rules: a clerk works on their own orders":
    use shop
    let o = as sam: Order(customer=anna, lines=[OrderLine(product=mug, qty=2 pcs)])
    as sam: o.notes = "Gift wrap"
    expect_error "permission denied: kim cannot edit Order.notes":
        as kim: o.notes = "Not mine"
    expect_error "permission denied: kim cannot run Order.cancel":
        as kim: o.cancel(reason="Not mine either")


test "field rules: who may change what":
    use shop
    as sam: anna.email = "anna.rossi@example.com"
    expect_error "permission denied: sam cannot edit Customer.credit_limit":
        as sam: anna.credit_limit = 900.00 EUR
    as alex: anna.credit_limit = 900.00 EUR
    expect_error "permission denied: sam cannot edit Product.price":
        as sam: mug.price = 1.00 EUR/pcs


test "separation of duties":
    use shop
    let o = Order(customer=anna, lines=[OrderLine(product=mug, qty=2 pcs)])
    o.confirm()
    let inv = as alex: o.create_invoice()
    as alex: inv.post()
    expect_error "permission denied: deny run Invoice.cancel (alex)":
        as alex: inv.cancel()
    as olivia: inv.cancel()
    expect inv.state is Cancelled
```

Notice how the error names the rule that refused: `deny run Invoice.cancel (alex)`.

```bash
quadra test --filter "rules"
```

```text
✓ row rules: a clerk works on their own orders
✓ field rules: who may change what

2 passed, 0 failed
```

```bash
quadra test
```

```text
…
32 passed, 0 failed
```

::: tip `hide` is about what people see
`hide` removes fields from what a role is shown — API answers, the web client, exports. It is
not a write rule: to stop a role from changing a field, don't give it `edit` on that field (as
`Clerk` does with `Customer.credit_limit`). You'll see hidden fields in the API in
[step 11](./11-database-and-api).
:::

## Try it

1. Give `Warehouse` the right to read customers' addresses but nothing else of the customer.
   (Hint: `read Customer` plus `hide` for the fields they shouldn't see.)
2. Add a `deny run Order.confirm where o => o.customer.tier is Standard and o.total > 300.00 EUR`
   rule and test it as `sam`. Does it stop `olivia` too?
3. Add an `Auditor` role with `read *` and `read history *`.

## Recap

- Roles list rules: verbs (`read`, `create`, `edit`, `run`, `all`, `hide`, `show`), targets
  (entities, fields, actions, `*`) and an optional row condition with `me`.
- Nothing is allowed by default; lines follow their document; top-level `deny` rules apply to
  everybody.
- `as login:` runs test statements with a user's permissions; errors name the rule that refused.

[Next: ledgers →](./10-ledgers)
