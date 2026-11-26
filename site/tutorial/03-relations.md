# Step 3: Relations

<SandboxLink project="tutorial-03" file="orders.qd" />

**Goal:** connect things. Products belong to categories and come from suppliers; customers
refer other customers; and, most importantly, customers place **orders**, each with several
**lines**.

The project grows, so we split it into modules: `customers.qd`, `catalog.qd` and `orders.qd`.

## References and inverses

A field whose type is an entity is a **reference** (many-to-one). A field whose type is a
**list** of entities is the other side of a reference (one-to-many): Quadra finds the
reference that points back and keeps the list up to date for you.

```quadra
entity Category:
    name: Text unique
    products: [Product]                     # the inverse of Product.category


entity Product:
    code: Code unique
    name: Text
    description: Text?
    category: Category?
    # …
```

When an entity has two references to the same entity — or one to itself — say which one the
list follows with `via`:

```quadra{17-18}
# The people we sell to.

record Address:
    street: Text
    city: Text
    zip: Text


entity Customer:
    name: Text unique
    email: Email?
    address: Address?                       # a record: stored inside the customer
    since: Date default today()
    discount: Percent default 0%
    credit_limit: Money<EUR> default 500.00 EUR

    referred_by: Customer?                  # who brought them to us
    referrals: [Customer] via referred_by   # the customers they brought
```

`record Address` is a **record**: a group of fields with no identity of its own, stored inside
the customer (an address is not shared, it is copied).

## Many-to-many

A **set** `{T}` is a simple many-to-many. When the link has data of its own — what a supplier
charges for a product, and how long it takes — make the link an entity and jump over it with
`through`:

```quadra
entity Product:
    # …
    tags: {Tag}                             # a set: many-to-many

    offers: [SupplierOffer]
    suppliers: {Supplier} through SupplierOffer


entity Tag:
    name: Text unique
    products: {Product} via tags


entity Supplier:
    name: Text unique
    email: Email?
    offers: [SupplierOffer]


# Many-to-many with data of its own: what a supplier charges us for a product.
entity SupplierOffer:
    product: Product
    supplier: Supplier
    cost: Price<EUR, product.uom>
    lead_time: Duration default 7 days
    unique (product, supplier)
```

`cost: Price<EUR, product.uom>`: units can follow a path through a reference, too.
`unique (product, supplier)` is a composite uniqueness rule.

## Documents with lines: `parent`

An order is a **document**: a header and its lines. The line points to its order with
`parent`, which means *composition*: the line lives and dies with the order, and reading or
changing a line is reading or changing the order (permissions and frozen documents, later,
follow this).

```quadra
# Orders: a document with lines.

from customers import Customer, Address
from catalog import Product


entity Order:
    customer: Customer
    date: Date default today()
    ship_to: Address? default customer.address
    notes: Text?
    lines: [OrderLine]


entity OrderLine:
    order: Order parent                     # the line lives and dies with its order
    product: Product
    qty: Qty<product.uom>
    price: Price<EUR, product.uom> default product.price


# Customer is declared in customers.qd, which cannot import this module (no cycles):
# we add the inverse of Order.customer from here.
extend Customer:
    orders: [Order]
```

Three things to notice:

- `from customers import Customer, Address` — a module sees only what it imports (plus the
  standard library). The compiler tells you what to import:
  ``unknown type `Customer`: it is declared in `customers`, add `from customers import Customer` ``.
- `default customer.address` and `default product.price` **copy** a value when the record is
  created. If the product's price changes tomorrow, today's order lines keep today's price.
- `extend Customer` adds a field to an entity declared in another module. `customers.qd` can't
  import `orders.qd`, because `orders.qd` already imports `customers.qd`:

```text
error: import cycle: customers → orders → customers (use `extend` in the higher module instead)
```

## Optional values: `?.` and `??`

`category: Category?` may be `none`, and the compiler won't let you forget it. Reading
`o.ship_to.city` when `ship_to` is optional does not compile:

```text
error: `o.ship_to` is optional: use `?.` (or check it is not none first)
   ┌─ orders.qd:28:12
   │
28 │     return o.ship_to.city
   │            ^^^^^^^^^^^^^^
```

- `o.ship_to?.city` reads through: it is `none` if `ship_to` is `none`.
- `x ?? fallback` gives a value when `x` is `none`.
- `if let a = o.ship_to:` runs a block only when the value is there, with `a` not optional.

## Tests

`tests/relations.test.qd` creates a small world and walks it in every direction:

```quadra
test "categories, suppliers and tags":
    let mugs = Category(name="Mugs")
    let mug = Product(code="MUG-01", name="Espresso mug", category=mugs, price=7.50 EUR/pcs)
    let beans = Product(code="BEANS-01", name="House blend", uom=kg, price=18.00 EUR/kg)
    expect mugs.products == [mug]                   # inverse, kept up to date for us

    # optionals: `?.` reads through a value that may be none, `??` gives a fallback
    expect mug.category?.name == "Mugs"
    let label = beans.category?.name ?? "Uncategorized"
    expect label == "Uncategorized"

    let roastery = Supplier(name="Roastery Bros")
    SupplierOffer(product=beans, supplier=roastery, cost=9.00 EUR/kg)
    expect beans.suppliers == {roastery}            # straight through SupplierOffer
    expect roastery.offers.single().lead_time == 7 days

    let gift = Tag(name="gift idea")
    mug.tags = {gift}
    expect gift.products == {mug}


test "an order with lines":
    let mug = Product(code="MUG-01", name="Espresso mug", price=7.50 EUR/pcs)
    let beans = Product(code="BEANS-01", name="House blend", uom=kg, price=18.00 EUR/kg)
    let anna = Customer(name="Anna Rossi",
                        address=Address(street="Via Roma 1", city="Milano", zip="20121"))
    let bob = Customer(name="Bob Smith", referred_by=anna)
    expect anna.referrals == [bob]

    let o = Order(customer=anna, lines=[
        OrderLine(product=mug, qty=2 pcs),
        OrderLine(product=beans, qty=500 g),
    ])
    expect o.ship_to?.city == "Milano"              # copied from the customer
    expect o.lines.count() == 2
    expect o.lines[1].qty == 0.5 kg                 # stored in the product's unit
    expect o.lines[1].price == 18.00 EUR/kg         # copied from the product
    expect anna.orders == [o]
```

Lines are created inline, inside the order: `Order(customer=anna, lines=[OrderLine(…), …])`.
`lines[1]` is the second line; `.single()` is the only element of a list (and fails if there
isn't exactly one).

::: tip `??` and comparisons
`??` binds tighter than comparisons: `beans.category?.name ?? "Uncategorized" == "Coffee"`
compares the name, or the fallback, with `"Coffee"`.
:::

```bash
quadra test
```

```text
✓ categories, suppliers and tags
✓ an order with lines
✓ a new customer
✓ new products are active
✓ prices, quantities and units
✓ money is exact
✓ dates and durations
✓ the compiler catches unit and currency mistakes

8 passed, 0 failed
```

## Try it

1. Add `parent: Category?` and `children: [Category] via parent` to `Category`, and test a
   two-level tree.
2. Add `preferred_supplier: Supplier?` to `Product` and its inverse
   `preferred_by: [Product]` to `Supplier`, then test it.
3. Write a function `fn city(o: Order) -> Text` that returns the shipping city, or `"pickup"`
   when there is no address.

## Recap

- A reference is a field of an entity type; a list `[T]` is its inverse; `via` picks the
  reference when there is more than one.
- `{T}` is a many-to-many set; `through` jumps over a link entity that has data of its own.
- `parent` makes lines part of their document.
- `record` is a value with fields but no identity.
- Optionals need `?.`, `??` or `if let`; the compiler checks it.
- Modules import what they use; `extend` adds fields from a higher module, without cycles.

[Next: formulas →](./04-formulas)
