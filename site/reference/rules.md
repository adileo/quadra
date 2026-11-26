# Rules

Rules state what must always be true of the data. The engine verifies them at the end of
every transaction: if one fails, nothing of the transaction is written. This page covers
`check`, `warn`, uniqueness, `required when`, `where`, `frozen when … except … live …`, and
the choice between a default and a formula.

| Rule | Blocks the transaction | Page section |
|---|---|---|
| `check cond, "message"` | yes | [check](#check) |
| `warn cond, "message"` | no: the message is attached to the record | [warn](#warn) |
| `unique (a, b) [when cond]`, `field: T unique` | yes | [unique](#unique) |
| `field: T? required when cond`, a non-optional `field: T` | yes | [required](#required-values) |
| `field: T where …` | yes | [where](#where) |
| `check per (keys): g => …` | yes (ledgers) | [Ledgers](/reference/ledgers#per-group-checks) |
| `frozen when cond` | yes: changes to a frozen record are refused | [frozen](#frozen) |

## When rules are checked

A transaction is one action called from the API, one statement of a test, one fixture, one
migration or one change. When its code has run, the engine:

1. computes the pending defaults;
2. applies automatic transitions, `becomes` reactions and ledger postings, until nothing
   changes;
3. unfreezes the frozen records whose `frozen when` no longer holds (a transition does it as
   it happens);
4. verifies every rule: required values, `where`, `check`, `warn`, `unique`, ledger checks;
5. freezes the records whose `frozen when` became true.

If a rule fails, the whole transaction is undone, including what reactions did, and the
message of the first failure is returned. Missing required values are reported together, in
the order the records were created:

```text
DocLine.q is required; Doc.total is required
```

With a database, rules are evaluated incrementally: only on the records that a change can
affect. A rule that reads the clock (`today()`, `now()`) can become false without any data
changing; such rules are re-evaluated on every record by a periodic sweep: see
`quadra db sweep` in [CLI](/reference/cli#quadra-db-sweep).

## `check`

```quadra
check lines.count() >= 2, "An entry needs at least two lines"
check lines.sum(l => l.balance) == 0, "Unbalanced entry"
check approved_by == none or approved_by != created_by, "Whoever creates the order cannot approve it"
check account.currency == none or account.currency == currency,
    f"Account {account.code} only accepts {account.currency.code}"
```

`check condition, message`: the condition must be true for every record of the entity. The
message is any text expression (often an f-string); inside it optional values can be read
without `?.`. A failed check fails the transaction with its message.

Checks are not evaluated on a record that misses a required value: the missing value is
reported instead.

## `warn`

```quadra
warn supplier_info != none or product.suppliers.count() == 0,
    "Quantity below the minimum order of every price list of the vendor"
warn price_gap <= 0.05, "Price differs from the order by more than 5%"
```

`warn` has the form of `check`, but does not block: the message is attached to the record.
It is listed in the record's `warnings` field (a document also shows its lines' warnings), in
the `warnings` of every row returned by the [API](/reference/http-api), and in the web client.

## `unique`

```quadra
entity Account:
    company: Company default current.company
    code: Code
    unique (company, code)

entity Partner:
    vat_number: VatNumber?
    kind: enum(Organization, Person) default Organization
    unique (vat_number) when kind is Organization
```

| Form | Meaning |
|---|---|
| `field: T unique` | no two records have the same value |
| `unique (a, b, …)` | no two records have the same combination |
| `unique (a, …) when cond` | only among the records for which `cond` holds |

Records with `none` in one of the fields are not compared. Messages, the first for a
`unique` field or a one-field `unique (n)`, the second for several fields or a `when`:

```text
User.login must be unique: giulia already exists
unique (vat_number): IT01234567890 already exists in Partner
```

Without `when`, a uniqueness rule is also a `UNIQUE` constraint of the entity's table in the
database (for fields stored in a single column).

## Required values

A field with a non-optional type (`name: Text`) must have a value when the transaction ends:
`Customer.name is required`. A field with a default always has one.

An optional field can become required under a condition:

```quadra
province: Code? required when country.code == "IT"
approved_by: User? required when state is Confirmed and needs_approval
rate: Decimal? required when state is not Draft
```

Where the field is read, the type checker treats it as `T`, not `T?`: the engine guarantees
the value whenever it is needed, so code can rely on it (for instance `inv.rate` on a
posted invoice). If the condition is false, reading it can still give `none`.

## `where`

`where` restricts the values of a field: see [Entities](/reference/entities#modifiers).

```quadra
qty: Qty<uom> where > 0
decimals: Int where 0..4 default 2
discount: Percent where 0%..100% default 0%
code: Code where c => c.len() == 2 unique
journal: Journal where j => j.company == company
```

A violation fails the transaction: `` Partner.discount = 150%: does not satisfy its `where` ``.

Like the other rules, a `where` is verified again whenever something it reads changes: with
`product: Product where p => p.active` on an order line, deactivating a product fails while a
draft order uses it. On a [frozen](#frozen) record (and on the lines of a frozen document) it
is not: it held when the record was frozen, and the field cannot change since. Deactivating
a product sold in a confirmed order is allowed. A field listed in `except` is checked when it
is written.

## Frozen

```quadra
entity Invoice is Document:
    state: enum(Draft, Posted, Cancelled) default Draft
    …
    frozen when state is not Draft live paid, due, payment_state
```

`frozen when condition [except a, b] [live x, y]`. At the end of the transaction in which the
condition becomes true, the record is **frozen**, and it stays frozen while the condition
holds:

| What | On a frozen record |
|---|---|
| Stored fields | cannot change: `` Invoice is frozen (state is not Draft): `total` cannot change `` |
| Fields listed in `except` | can still change |
| The field of a `flow` | still moves with the flow (actions and automatic transitions) |
| Lines (children with a `parent` field) | frozen with it: `Invoice is frozen (state is not Draft): its InvoiceLine lines cannot change`; their formulas read the same snapshot |
| Formulas | read the world **as it was when the record was frozen**: rates, prices, master data, other records |
| Formulas listed in `live` | keep following the current data |
| `where` of its fields | not checked again ([where](#where)); `check` rules and required values still are |

The snapshot is what makes a posted invoice stay the same when a tax rate or a price list
changes later:

```quadra
as anna: inv.post()
iva22v.rate = 24%
expect inv.tax == 2.20 EUR      # still 22%: the snapshot at posting
```

A frozen record cannot be corrected by assignment: corrections go through the actions of
the process (a credit note and a new invoice), so accounting and numbering stay consistent.

A `check` of a frozen record keeps guarding what can still change: `check due >= 0` on an
invoice whose `due` is `live` refuses a payment that pays more than the total.

### Unfreezing

When a transition makes the condition false, the record is no longer frozen: it and its lines
can be edited again, and its formulas follow the current data. The history is kept (the
freezing time is cleared, the values written before stay as they were):

```quadra
frozen when state is not Draft
flow state:
    Draft -> Confirmed      by confirm
    Confirmed -> Draft      by reopen

action reopen:
    reopened_at = now()      # allowed: the action leaves the frozen states
```

- The action that performs such a transition may write the record in its body: the record is
  unfrozen before the body runs. The `on Order enters Draft` reactions see it unfrozen too.
- An action between two frozen states (`Confirmed -> Locked by lock`) does not unfreeze: its
  body may write only the fields in `except`.
- Reopened, the record must satisfy its rules again, `where` included: the transition fails
  if it does not (`` OrderLine.product = Pen: does not satisfy its `where` ``).
- The next transition into a frozen state freezes it again, with a new snapshot.

### `live`

A frozen document often has values that must keep following the data that comes after it:
the payments of an invoice, the deliveries or invoices of a confirmed order. List those
formulas as `live`:

```quadra
frozen when state in {Confirmed, Locked, Cancelled} except notes live deliveries, delivery_status
```

`live` can also be written as a member of its own, in the entity or in an `extend` (useful
when the formula is added by another module). The lines of a frozen document declare their
own `live` formulas this way, in the line entity:

```quadra
entity SalesLine:
    order: SalesOrder parent
    …
    # frozen with the order; the deliveries arrive later
    live delivered, to_deliver
    moves: [StockMove] via sale_line
    delivered = moves.where(m => m.state is Done).sum(m => m.qty.to(uom))
    to_deliver = qty - delivered

extend SalesOrder:
    # the order is frozen at confirmation; invoices arrive later
    live invoices, invoice_status
    invoices = lines.flat_map(l => l.invoice_lines).map(i => i.invoice).distinct()
    invoice_status: enum(Nothing, Invoiced, ToInvoice) = when:
        state not in {Confirmed, Locked}  => Nothing
        lines.all(l => l.to_invoice <= 0) => Invoiced
        else                              => ToInvoice
```

A live formula that reads another formula of the same record sees that formula's frozen
value, unless it is live too. List every formula of the chain:
`live paid, due, payment_state` (where `due = total - paid`). The same holds across a
document and its lines: a live `delivery_status` of the order that reads `l.to_deliver`
needs `to_deliver` (and `delivered`) live on the line.

| Error | Cause |
|---|---|
| `` `live y`: Plain is never frozen (it has no `frozen when`, and it is not a line of a document that has one) `` | `live` on an entity that is not frozen, nor a line of a frozen document |
| `` `live note`: only formulas can be live (`note` is stored; to change it on a frozen record use `except`) `` | `live` on a stored field |
| `` `x` is not a field of Order `` | an unknown name in `except` or `live` |

### The frozen/live warning

A formula of a frozen document, or of one of its lines, that gathers records pointing to it
(invoices of an order, payments of an invoice, stock moves of an order line) would stop
seeing the ones created after it was frozen. That is almost always a mistake, so
`quadra check` warns, with the fix:

```text
warning: `invoiced` reads Invoice (via order): records that point to the Order and come after it is frozen. On a frozen Order it would keep the value it had when frozen; if it should follow them, declare `live invoiced`
warning: `delivered` reads StockMove (via sale_line): records that point to the SalesLine and come after its SalesOrder is frozen. On a line of a frozen SalesOrder it would keep the value it had when frozen; if it should follow them, declare `live delivered`
```

When the formula is declared in an `extend`, the message ends with
``declare `live billed` in this `extend` ``.

The warning is not given:

- for formulas that read the document's own lines (children whose back-reference is
  `parent`): they are frozen with it;
- for formulas already listed in `live` or `except`;
- when the record is frozen only in final states, that is states with no transition out of
  them (`frozen when state in {Done, Cancelled}`): nothing happens to it afterwards.

## Defaults and formulas

| | `field: T default expr` | `field = expr` |
|---|---|---|
| Computed | once, when the record is created | whenever it is read |
| Can be changed | yes, by users and actions | no: `` `amount` is a formula: it cannot be assigned `` |
| Follows later changes of what it reads | no: it is a snapshot | yes |
| May use `today()`, `now()`, `convert()` without a date | yes | no |
| Stored | yes | materialized in a column of the table, kept up to date |

```quadra
price: Price<EUR, pcs> default product.price          # the list price at order time
amount = (qty * price * (1 - discount)).round()       # always qty × price
```

### Formula cycles

Formulas may not depend on each other in a circle:

```text
error: formula cycle: Move.value → Product.cost → Move.value (break it with a `default` snapshot, or make one value stored)
```

A `default` breaks the cycle, because a snapshot does not depend on later values. This is how
an average cost (which depends on stock values) and the value of a stock move (which depends
on the cost) coexist:

```quadra
unit_cost: Price<company.currency, product.uom> default product.cost[company].round()
value = (qty_base * unit_cost).round()
```

Recursion over data is allowed when the data is guaranteed acyclic: if the entity has a
`check` that uses `closure(…)`, a formula may refer to itself through other records.

```quadra
entity Category:
    name: Text
    parent: Category?
    path = f"{parent.path}/{name}" if parent != none else name
    check not self.closure(c => c.parent).contains(self), "Cyclic category tree"
```

Without the `check`: `formula cycle: Category.path → Category.path (…)`.
