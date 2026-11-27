# Entities

Entities are the data of the application: each one becomes a table in the database, a
resource of the API and a sheet in the web client. This page covers every declaration that
defines data: `entity`, `record`, `trait`, `extend`, `enum`, fields and their modifiers,
relations, `temporary` entities, `seed`, `const`, functions and events.

| Declaration | What it is |
|---|---|
| `entity Name:` | Records with an identity, stored and versioned (every change is kept). |
| `record Name:` | A composite value without identity (an address), stored inside the record that holds it. |
| `trait Name:` | Fields shared by several entities, and a type that can refer to any of them. |
| `extend Name:` | Adds members to an entity declared in another module. |
| `enum Name: A, B` | A closed set of values, which may carry data. |
| `ledger Name:` | An append-only journal fed by `post to`. See [Ledgers](/reference/ledgers). |
| `temporary entity Name:` | A staging table that exists only while a migration runs. |
| `seed:` | Reference data created with the database. |
| `const NAME: T = value` | A named constant. |
| `fn name(…) -> T:` | A pure function. |
| `event Name(field: T, …)` | A message for the outside world, sent with `emit`. |

## `entity`

```quadra
entity Customer:
    name: Text unique
    email: Email?
    credit_limit: Money<EUR> default 1_000.00 EUR
    orders: [Order]

    open_total = orders.where(o => o.state is Confirmed).sum(o => o.total)
    check open_total <= credit_limit, "Credit limit exceeded"
```

An entity's body is a list of **members**:

| Member | Page |
|---|---|
| fields and formulas | [below](#fields) |
| `check`, `warn`, `unique (…)`, `frozen when …`, `live …` | [Rules](/reference/rules) |
| `flow field:` and `action name:` | [Flows and actions](/reference/flows-and-actions) |
| `post to Ledger:` | [Ledgers](/reference/ledgers) |
| `fn name(…):` | [methods](#functions) |
| `load from "file.csv"` | [temporary entities](#temporary-entities) |

`entity Name is Trait1, Trait2:` makes the entity implement [traits](#trait).

### Implicit fields

Every entity has these fields without declaring them:

| Field | Type | Value |
|---|---|---|
| `id` | `Text` | the record's identifier |
| `created_at` | `DateTime` | when it was created (the clock of the transaction) |
| `created_by` | `User?` | who created it; `none` for the system |
| `updated_at` | `DateTime` | when it was last written (the clock of that transaction); `created_at` until then |
| `updated_by` | `User?` | who last wrote it; `none` for the system |
| `version` | `Int` | the number of values written to it so far |
| `warnings` | `[Text]` | the messages of its failed [`warn`](/reference/rules#warn) rules (and of its lines') |

The whole history of every field, with the cause of each change, is kept as well: see
`why()` in [Expressions](/reference/expressions#built-in-functions) and the history endpoint
of the [HTTP API](/reference/http-api#reading).

The history is not a value in expressions: `rec.history`, `rec.as_of(…)` and
`rec.explain()` are compile errors that say where to find it (`why(rec)` for the cause, the
history endpoint of the API for the past values).

## Fields

| Form | Meaning |
|---|---|
| `name: T` | A stored field. Required: a record cannot be saved without a value. |
| `name: T?` | An optional stored field: it may be `none`. |
| `name: T default expr` | A stored field with an **initial value**, computed once when the record is created. It can be changed afterwards. |
| `name = expr` | A **formula**: always up to date, read-only, its type inferred. |
| `name: T = expr` | A formula with a declared type, checked against the expression. |
| `name default expr` | No type: a new initial value for a field inherited from a trait. |

Lists (`[T]`) and sets (`{T}`) are never required: an absent collection is empty.

```quadra
entity OrderLine:
    order: Order parent
    product: Product
    qty: Qty<pcs> where > 0
    price: Price<EUR, pcs> default product.price     # snapshot of the list price
    discount: Percent where 0%..100% default 0%
    amount = (qty * price * (1 - discount)).round()  # always follows qty, price, discount
```

The difference between `default` and `=` is the most important decision when modelling: a
formula follows the data forever, a default takes a picture once. See
[Defaults and formulas](/reference/rules#defaults-and-formulas).

A formula is stored in a column of its entity's table. Most formulas (rollups over lines,
rollups of rollups, values read through references, recursions over a tree) are kept up to
date by each commit and read from that column, so opening a region or sorting customers by
revenue does not walk their orders: see
[Formulas kept up to date](/reference/storage#formulas-kept-up-to-date).

### Modifiers

Modifiers follow the type, in any order:

| Modifier | Meaning | Example |
|---|---|---|
| `where > x` (also `>=`, `<`, `<=`, `==`, `!=`) | The value must compare that way to `x`. | `qty: Qty<pcs> where > 0` |
| `where a..b` | The value must lie between `a` and `b`, both included. | `decimals: Int where 0..4 default 2` |
| `where v => cond` | The value must satisfy the condition. For a list or a set, each element must. | `code: Code where c => c.len() == 3` |
| `required when cond` | On an optional field: a value is required while `cond` holds. Reading it gives `T`, not `T?`. | `vat_number: VatNumber? required when kind is Organization` |
| `unique` | No two records may have the same value (`none` excepted). | `login: Code unique` |
| `parent` | The record is a line of the referenced document (composition). | `order: SalesOrder parent` |
| `per company` | One value per company. | `list_price: Price<company.currency, uom> per company` |
| `via field` | The inverse of a collection goes through this field of the other entity. | `contacts: [Partner] via parent` |
| `through Entity` | A many-to-many relation through a linking entity. | `vendors: {Partner} through SupplierInfo` |

A `where` condition can read the other fields of the record and the referenced records:
`purchase_uom: Uom where u => u.category == uom.category`. Violations are reported when the
transaction ends, for example ``Product.hs_code = 1: does not satisfy its `where` ``. The
`where` on a reference also tells the type checker that two currencies are equal; see
[Types](/reference/types#how-the-checker-proves-two-currencies-equal).

Modifiers that do not fit on one line can go on the following lines, indented, together
with the `default` or the `=`:

```quadra
vendor: Partner? where v => v.as_vendor != none
    required when route is Buy
    default product.suppliers.sort_by(s => s.lead_time).first()?.vendor
```

### Per-company fields

A field marked `per company` holds a separate value for each company. In its type and its
formula, `company` is the company whose value is computed.

```quadra
extend Product:
    list_price: Price<company.currency, uom> per company
    stock_qty: Qty<uom> per company =
        Stock.where(s => s.product == self and s.company == company).sum(s => s.qty)
```

| Expression | Value |
|---|---|
| `product.list_price` | the value for `current.company` |
| `product.list_price[c]` | the value for company `c` (`none` if it has none) |

Writing `product.list_price = …` sets the value of the current company. A missing value of
a per-company field is not reported as missing.

## Relations

| Declaration | Relation |
|---|---|
| `customer: Customer` | many-to-one: each order has one customer |
| `customer: Customer?` | optional many-to-one |
| `orders: [Order]` | one-to-many: the orders whose `customer` is this record (the inverse is computed, not stored) |
| `contacts: [Partner] via parent` | one-to-many through a named field, when the inverse is ambiguous |
| `order: SalesOrder parent` | composition: the line belongs to the order |
| `tags: {Tag}` | many-to-many: a stored set of references |
| `partners: {Partner} via tags` | the inverse of a stored set |
| `vendors: {Partner} through SupplierInfo` | many-to-many through a linking entity with attributes |
| `origin: Document?` | a reference to any entity that implements the trait `Document` |

For `orders: [Order]`, the inverse field is the first field of `Order` whose type is the
owner (or one of the owner's traits). Use `via` to name it explicitly.

A list of child records can be given when the parent is created; their back-reference is set
automatically:

```quadra
let o = Order(customer=anna, lines=[OrderLine(product=pen, qty=4 pcs)])
expect o.lines.single().order == o
```

### Composition: `parent`

A field marked `parent` makes the record a line of its document. A line:

- is frozen with its document ([`frozen when`](/reference/rules#frozen)): it cannot change,
  its formulas read the data as of the freezing (except those it declares `live`), and it is
  editable again when the document is unfrozen;
- follows the permissions of its document: reading or changing a line is reading or changing
  the document ([Security](/reference/security#lines-follow-their-document));
- is copied with its document by `copy()`.

Lines can be read without `?.`: `l.order.customer`.

## `record`

(A record can also describe a document or a file of another system, read and written as
JSON, XML, CSV or fixed-width lines: `record ShopOrder from json:`, `record PosRow from
csv(sep=";"):`, with `at "…"` on its fields. See
[Formats](/reference/connections#formats-record-from).)

A record is a value with named parts and no identity of its own: two equal addresses are
the same address. It is stored inside the entity that holds it.

```quadra
record Address:
    street: Text
    city: Text
    zip: Text
    country: Country

entity Partner:
    name: Text
    address: Address
    delivery_addresses: [Address]
```

```quadra
let p = Partner(name="Rossi Spa",
                address=Address(street="Via Roma 1", city="Milano", zip="20100", country=it))
expect p.address.city == "Milano"
```

In the field types of a record, `owner` is the record or entity that holds it:

```quadra
record CustomerRole:
    credit_limit: Money<owner.currency>?     # the currency of the Partner holding the role
```

The rules written inside a `record` (`where`, `required when`, `check`, `warn`) are checked
when the record that holds it is saved, also for each element of a list of records and for
records inside records; a `warn` goes to the holding record:

```quadra
record PostalAddress:
    street: Text
    zip: Text where z => z.len() == 5
    country: Code default "IT"
    province: Code? required when country == "IT"
```

```text
PostalAddress.zip = 1: does not satisfy its `where`
```

## `trait`

A trait declares fields (and their defaults) that several entities share. An entity that
implements a trait has its fields as if declared in the entity.

```quadra
trait Document:
    company: Company default current.company
    number: Code?
    date: Date default today()

entity Invoice is Document:
    number default next_number(company, "INV")   # a new default for an inherited field
    total: Money<company.currency>
```

A trait is also a type: a field of type `Document` can refer to any entity that implements
it, and in the database the reference is a real foreign key. Test which entity it is with
`is` and take it apart with `match`:

```quadra
origin: Document?
check origin is not SalesLine or direction is Out, "A sales line on a vendor bill"

kind_label: Text = match origin:
    SalesOrder(o)    => "sale"
    PurchaseOrder(o) => "purchase"
    _                => "other"
```

`Document.all()` and `Document.where(…)` scan the records of every implementing entity (on a
database, a query over one entity is answered in SQL: see [Storage](./storage.md#loading-on-demand)).

`check`, `warn` and `unique (…)` written in a trait apply to every entity that implements it
(also through another trait). A `unique` holds among the records of each implementing
entity, as in the database, where every entity has its own table: `Document unique (company,
number)` lets an invoice and an order have the same number.
## `extend`

`extend` adds members to an entity declared elsewhere, typically in a lower module (or in the
standard library). The members behave as if they had been written in the entity.

```quadra
extend Company:
    vat_number: VatNumber unique
    po_approval_limit: Money<currency>?

extend SalesOrder:
    live invoices, invoice_status
    invoices = lines.flat_map(l => l.invoice_lines).map(i => i.invoice).distinct()

    action create_invoice -> Invoice:
        require state in {Confirmed, Locked}, "Only confirmed orders can be invoiced"
        …
```

`extend` accepts fields, formulas, `check`, `warn`, `unique (…)`, actions, functions,
`post to` and `live`, and two members only when the entity does not have them yet:

| Member | In an `extend` |
|---|---|
| `flow field:` | allowed on a field that has no flow; a second flow on the same field is an error: `` `SalesOrder.state` already has a flow `` (add the transitions to the existing one) |
| `frozen when …` | allowed if the entity has none; otherwise an error: `` `SalesOrder` already has `frozen when` `` (declare the new fields `live`, or list them in `except` where the entity is declared) |
| `load from` | an error: it belongs in the declaration of a [temporary entity](#temporary-entities) |

An entity of the standard library (`Company`, `User`, `Sequence`, `Currency`, …) is extended,
never declared again: `entity Company:` in a project is an error, `` `Company` is declared in
the standard library (`std.org`): use `extend Company:` ``.

## `enum`

```quadra
enum ProductType: Storable, Consumable, Service

enum AccountType:
    Receivable, Payable, Bank, Cash
    Income, Expense

enum PriceTarget:
    AllProducts
    OneProduct(product: Product)
    InCategory(category: Category)
```

An enum can also be written inline, as the type of a field. This is the usual way to declare
the states of a document:

```quadra
state: enum(Draft, Confirmed, Done, Cancelled) default Draft
```

Variants are written by their bare name (`Draft`, `Storable`); the type the context expects
decides which enum they belong to. A variant that does not belong to the expected enum is an
error: `` `Archived` is not a value of enum(Draft, Confirmed, Done, Cancelled) ``.

Test a value with `is` and `in`, or take it apart with [`match`](/reference/expressions#match),
which must cover every variant:

```quadra
specificity: Int = match target:
    OneProduct(_) => 3
    InCategory(_) => 2
    AllProducts   => 1
```

A variant that carries data is constructed like a call, with its fields by name or in the
declared order: `OneProduct(product=p)` or `OneProduct(p)`. A missing field
(`` `OneProduct` needs `product` ``), an unknown one (`` `OneProduct` has no field `x` ``) and
arguments to a variant without data (`` `AllProducts` has no fields ``) are compile errors.

## Creating records

An entity name called with named arguments creates a record:

```quadra
let anna = Customer(name="Anna Bianchi")
let o = Order(customer=anna, lines=[OrderLine(product=pen, qty=4 pcs, discount=10%)])
```

| Rule | Detail |
|---|---|
| Named arguments only | `` `Line(…)` takes named arguments: `Line(field=value)` `` |
| Fields only | `` `Line` has no field `nope` ``; a formula cannot be set: `` `amount` is a formula: it cannot be set ``; the field of a flow takes only its initial state ([Flows](/reference/flows-and-actions#the-state-changes-only-through-the-flow)) |
| Defaults | Fields not given take their `default`, computed at the end of the statement (or when first read), so a default can use the other fields given and other defaults. |
| Validation | `where`, `required`, `unique` and `check` rules are verified when the transaction ends, not at each assignment. |
| Traits | A trait cannot be created: `` `Document` is a trait: create one of its entities instead ``. |

### `copy()`

`record.copy(field=value, …)` creates a new record with the same stored values, then applies
the given values. It does not copy formulas, the state of a flow (it restarts from its
default), the field `number`, or fields that are `unique` on their own. Lines (children
whose back-reference is `parent`) are copied too.

```quadra
let credit = inv.copy(kind=CustomerCredit, reverses=inv, date=today(), notes=reason)
```

There is no delete: a record that should no longer count is cancelled, reversed or
deactivated by the model (a state, an `active` flag).

## Temporary entities

A `temporary entity` is a staging table for a [migration](/reference/migrations-and-changes):
its records exist only while the migration runs and are removed before it commits. With
`load from`, its records are read from a CSV file (path relative to the project root).

```quadra
temporary entity CustomsCode:
    product_code: Text
    code: Code where c => c.len() == 8
    load from "customs.csv"
```

```text
product_code,code
A,73181500
B,35069100
```

| CSV rule | Detail |
|---|---|
| Header | The first line names the fields, in any order. |
| Separator and quotes | `,` separates cells; `"…"` quotes a cell, with `""` for a quote inside it. |
| Ignored lines | Empty lines and lines starting with `#`. |
| Empty cell | `none`. |
| Values | `Int`, `Decimal`, `Bool` (`true`, `yes`, `1`), `Date` (`YYYY-MM-DD`), enum variants by name, text otherwise. |
| References | A cell for a reference field is matched against the target's unique fields first, then its other stored fields: ``no Product matches `X` `` if nothing matches. |
| Errors | Reported with the file's path and the line: `` …/customs.csv line 3: `x` is not an integer ``. |

## `seed`

A `seed:` block creates reference data. Seeds run once, in the first transaction of a new
database (`quadra db init`), and before every test.

```quadra
seed:
    Location(name="Suppliers", kind=Supplier)
    Location(name="Customers", kind=Customer)
```

Seeds of `Currency` and `Uom` records also tell the type checker which currencies and units
exist (see [Types](/reference/types#where-currencies-and-units-come-from)).

## `const`

```quadra
const STANDARD_VAT: Percent = 22%
```

A constant has an optional type and a value, evaluated where it is used.

## Functions

```quadra
fn price_for(product: Product, pricelist: Pricelist?, qty: Qty, on: Date) -> Exact<Price>:
    if pricelist == none:
        return product.list_price
    …
```

| Form | Where | Notes |
|---|---|---|
| `fn name(p: T, q: T = default) -> R:` | top level | A function. Parameters may have defaults; the return type is checked against every `return`. |
| `fn name(…) -> R:` | inside an entity | A method: it sees the record's fields by their bare names and `self`. Called as `record.name(…)`. |
| `fn name(p, …) -> R:` | inside a block (reaction, test) | A local function; it sees the variables of the block. |
| `builtin fn name(…) -> R` | standard library | Implemented by the engine (`next_number`). |

```quadra
entity PaymentTermLine:
    term: PaymentTerm parent
    days: Duration default 0 days
    end_of_month: Bool default false

    fn due_from(start: Date) -> Date:
        let d = start + days
        return d.end_of_month() if end_of_month else d
```

Arguments are positional or named (`exchange_rate(USD, EUR, on=today())`). A `Money`
parameter without a currency carries its own: the result type can refer to it, and at the
call site it becomes the currency of the argument.

```quadra
fn split(amount: Money, start: Date) -> [(due: Date, amount: Money<amount.currency>)]:
    return lines
        .allocate(amount, by=l => l.percent)
        .map(a => (due: a.item.due_from(start), amount: a.share))
```

## Events

```quadra
event SendEmail(to: Email, template: Text, document: Document)
```

An event is a typed message for the outside world. It is sent with `emit` from an action or
a reaction; see [Flows and actions](/reference/flows-and-actions#events).

## Views and reports

`view` and `report` declarations are parsed and their names are checked, but the web client
does not draw them yet; the grammar is in `docs/GRAMMAR.md`.

```quadra
view SalesOrderForm(o: SalesOrder):
    header: o.number, o.customer, o.date, o.state
    table o.lines as l: l.product, l.qty, l.price, l.subtotal
    footer: o.untaxed, o.tax, o.total
    actions: o.send, o.confirm
```
