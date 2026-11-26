# Ledgers

A ledger is an append-only journal of movements: stock, general ledger, budget
consumption. Rows are never edited: records **post** to the ledger, and when what they post
changes, the engine writes reversal rows and new rows. Balances and stock levels are sums over
a ledger, not fields that someone updates.

```quadra
ledger Stock:
    key product: Product
    key location: Location
    key lot: Lot?
    measure qty: Qty<product.uom>
    measure value: Money<company.currency>
    key company: Company

    check per (product, location, lot): g => g.sum(r => r.qty) >= 0 or g.key.location.allow_negative,
        "Negative stock"

entity StockMove:
    company: Company default current.company
    product: Product
    qty_base: Qty<product.uom>
    value: Money<company.currency>
    source: Location
    dest: Location
    lot: Lot?
    state: enum(Draft, Ready, Done, Cancelled) default Draft

    post to Stock when state is Done:
        add(company=company, product=product, location=source, lot=lot, qty=-qty_base, value=-value)
        add(company=company, product=product, location=dest, lot=lot, qty=qty_base, value=value)

extend Product:
    on_hand: Qty<uom> = Stock.where(s => s.product == self and s.location.is_internal).sum(s => s.qty)
```

## Declaring a ledger

`ledger Name:` followed by its columns and checks.

| Member | Meaning |
|---|---|
| `key name: T` | a dimension of the movement (product, account, partner…); may be optional |
| `measure name: T` | a quantity that is summed and reversed (an amount, a quantity, a number) |
| `attr name: T` | a descriptive value that is neither a key nor summed (a due date) |
| `check per (k1, k2): g => cond, "message"` | a rule on each group of rows with the same keys |
| `check cond, "message"` | a rule on each row ([Row checks](#row-checks)) |

Every row also has two implicit columns:

| Column | Value |
|---|---|
| `source` | the record that posted the row |
| `date` | when it was posted (`DateTime`, the clock of the transaction) |

Types of measures may refer to keys: `measure amount: Money<company.currency>`,
`measure qty: Qty<product.uom>`.

## Posting: `post to`

```quadra
post to Ledger when condition:
    add(column=value, …)
```

A `post to` block in an entity describes the rows a record contributes to a ledger. The block
is ordinary code (`let`, `for`, `if`) that calls `add(…)` once per row; `when` is optional.

```quadra
entity JournalEntry is Document:
    lines: [JournalLine]
    state: enum(Draft, Posted) default Draft

    post to GL when state is Posted:
        for l in lines:
            add(company=company, account=l.account, partner=l.partner, currency=l.currency,
                due=l.due, amount=l.balance, amount_currency=l.amount_currency)
```

| Rule | Detail |
|---|---|
| Named columns | `add(product=…, qty=…)`; a positional argument: `add() takes named columns: add(product=…, qty=…)` |
| Known columns | `` ledger `Stock` has no column `bin` `` |
| Several blocks | An entity may post to several ledgers, or several times to the same one. |

### How rows are written

After every statement, together with the other automations, the engine computes for each
record what its `post to` blocks produce now, and compares it with what the record has
posted so far:

| Situation | Rows written |
|---|---|
| the condition becomes true | the new rows |
| the posted values change (a quantity, an account) | a reversal of the previous rows (every measure negated) and the new rows |
| the condition becomes false (a move is cancelled) | a reversal of the previous rows |
| nothing changed | nothing |

The ledger therefore keeps every movement and every correction, while its sums always equal
what the records currently say. A move of 10 pieces that is cancelled leaves two rows, `+10`
and `-10`.

Rows are written only by `post to`: ledgers cannot be created or changed from the API.

## Querying

The ledger name is the collection of its rows; every
[collection method](/reference/expressions#collection-methods) applies:

```quadra
balance = GL.where(g => g.account == self and g.company == company).sum(g => g.amount)

stock_value: Money<company.currency> per company =
    Stock.where(s => s.product == self and s.company == company and s.location.is_internal).sum(s => s.value)

cogs = Stock.where(s => s.company == company and s.location.kind is Customer and s.date in period).sum(s => s.value)

paid_by_payments = GL.where(g => g.company == c and g.source is Payment).sum(g => g.amount)
```

Filtering on a column with `==` (as in `s.product == self`) lets the engine load only the
matching rows from the database. Summing amounts requires one currency: filter on the key that
fixes it (`g.company == company`), see [Types](/reference/types#sums-over-records).

## Per-group checks

```quadra
check per (company, source): g => g.sum(r => r.amount) == 0, "Double entry violated"
check per (product, location, lot): g => g.sum(r => r.qty) >= 0 or g.key.location.allow_negative,
    "Negative stock"
```

`check per (keys): g => condition, message` groups the rows by the listed columns
(`source` may be one of them) and requires the condition for every group. In the lambda,
`g` is the list of rows of the group and `g.key.<column>` is the group's value of a key
column. The check runs at the end of every transaction that posts to the ledger; with a
database, only on the groups that received rows.

When it fails, the transaction is undone, including the action that caused the posting:

```quadra
let big = Order(customer=anna, lines=[OrderLine(product=pen, qty=7 pcs)])
big.confirm()
expect_error "Not enough stock":
    big.ship()
expect big.state is Confirmed          # the transaction was rolled back
```

## Row checks

A plain `check` in a ledger is a rule on each row: its columns are names, as the fields of a
record are in an entity's checks.

```quadra
ledger GL:
    key account: Account
    measure amount: Money<company.currency>
    key company: Company
    check amount != 0, f"A movement of nothing on account {account.code}"
```

It holds on every row a record posts, at the end of the transaction that posts it; when it
fails, the transaction is undone. The automatic reversals are not checked again: they undo
rows that were checked when they were posted (a rule on the sign of a measure, which a
reversal flips, belongs in `check per`). A ledger cannot have `warn` rules: rows are never
edited, so a warning could never be resolved.
