# Step 12: Migrations and changes

<SandboxLink project="tutorial-12" file="migrations/2026-12-01_sku_and_origin.qd" />

**Goal:** the shop has been running for a while, and the model must change under real data:

- product `code`s are to be called `sku`s, like everywhere else in the industry;
- the payment method, free text until now, must become a closed list — the data contains
  `"Card"`, `"card"`, `"bank transfer"`…;
- customs need the **country of origin** of every product: a new required field, for
  products that already exist;
- and, separately, coffee prices go up 5% — a bulk change of data that someone must approve.

Each of these is easy to get wrong in a classic stack. Quadra has two tools for them:
**migrations** for changes of the model, and **changes** for changes of the data.

## The new model

In `catalog.qd`, `code` becomes `sku`, and `origin` is new:

```quadra{2,5}
entity Product:
    sku: Code unique                        # was `code`: see migrations/2026-12-01_sku_and_origin.qd
    name: Text
    description: Text?
    origin: Code where c => c.len() == 2    # country of origin (ISO 3166), required for shipping abroad
    category: Category?
    # …
    label = f"{sku} · {name}"
```

In `invoices.qd`, the payment method is an enum:

```quadra{5}
entity Payment:
    invoice: Invoice
    date: Date default today()
    amount: Money<EUR> where > 0
    method: enum(Card, Transfer, Cash) default Card     # was free text: see migrations/
```

Tests and fixtures follow: `Product(sku="MUG-01", origin="IT", …)`, `method=Transfer`.
`quadra check` and `quadra test` pass: the code is consistent. But the **database** still has
the old shape:

```bash
quadra db status
```

```text
sqlite:/…/shop/.quadra/data.db
  schema      b032bbb55687b2c6 (CHANGED: run `quadra db migrate`)
  migrations  1 pending: sku_and_origin
  commits     5
  last        a460af102d66048d2ef3c03bb20899a4079874491506d7b0641101569d09751a
```

## A migration

Without help, migrating would fail — the existing products have no `sku` and no `origin`:

```text
error: the existing data does not satisfy the new model: Product.sku is required; Product.origin is required
```

A `migration` block goes with the code change and tells Quadra how to bring the data along.
Create `migrations/2026-12-01_sku_and_origin.qd`:

```quadra
# A migration travels with the code change it belongs to (same release).
# `quadra db migrate --dry-run` runs it on the live data without writing; `quadra db migrate`
# runs it in the same transaction that brings the tables to the new model.

migration sku_and_origin:
    reason: "SKUs instead of codes; customs need the country of origin; payment methods become a closed list"

    # 1. A rename: the history is not rewritten, old entries are read under the new name.
    rename Product.code -> Product.sku

    # 2. A change of type: every stored value goes through these arms (`old` is the previous model).
    convert Payment.method from old.Text:
        "card" | "Card"                 => Card
        "transfer" | "bank transfer"    => Transfer
        "cash"                          => Cash
        other                           => fail f"Unknown payment method: {other}"

    # 3. A new required field: fill it from a file, or with a provisional value.
    backfill Product.origin:
        for p in Product.all():
            if let origin = ProductOrigin.find(x => x.sku == p.sku)?.origin:
                p.origin = origin
            else:
                p.origin = provisional("XX", reason="Ask the supplier", until=2027-01-31)

    # 4. If this does not hold, nothing is written.
    verify:
        Product.count(p => p.origin.is_provisional) <= 5


# Data that exists only while the migration runs.
temporary entity ProductOrigin:
    sku: Code
    origin: Code
    load from "migrations/origins.csv"
```

and the data file `migrations/origins.csv`:

```text
sku,origin
MUG-01,IT
BEANS-01,BR
```

Step by step:

- **`rename`** records that `Product.code` is now `Product.sku`. The history is never
  rewritten — past commits stay signed as they were — and entries written before the rename
  are read under the new name.
- **`convert … from old.Text`** maps every stored value with `match`-like arms. `other` catches
  the rest; `fail` refuses it with a message.
- **`backfill`** runs code to fill a new field. Here it looks the origin up in a
  **`temporary entity`** loaded from a CSV file, which disappears after the migration.
- **`provisional(value, reason, until)`** stores a value that satisfies the type and the rules
  (the schema stays valid) but is *marked* as provisional, with a reason and a deadline. After
  the deadline, any transaction that touches that record is refused until someone sets a real
  value.
- **`verify:`** — conditions that must hold at the end, or the whole migration is refused.

## Dry run, then migrate

Always run the migration on the real data without writing first:

```bash
quadra db migrate --dry-run
```

```text
  migration sku_and_origin
    rename Product.code → Product.sku: 3 values (the history keeps the old name)
    convert Payment.method: 2 values
    backfill Product.origin: 3 values, 1 provisional
    verify: ok
  rename column product.code → sku
  copy table payment
  add column product.origin
  copy table product (its new NOT NULL columns)
  every Payment: 1 new or changed rule
  every Product: formulas label, 2 new or changed rules
✓ dry run: the data satisfies the new model; nothing written
```

Had we forgotten a spelling of "bank transfer", the dry run would have told us, with the
record:

```text
error: the existing data does not satisfy the new model: migration sku_and_origin: convert Payment.method: 1 values cannot be converted:
  Payment#46: Unknown payment method: bank transfer
```

Now for real:

```bash
quadra db migrate
```

```text
  migration sku_and_origin
    rename Product.code → Product.sku: 3 values (the history keeps the old name)
    convert Payment.method: 2 values
    backfill Product.origin: 3 values, 1 provisional
    verify: ok
  rename column product.code → sku
  copy table payment
  add column product.origin
  copy table product (its new NOT NULL columns)
  every Payment: 1 new or changed rule
  every Product: formulas label, 2 new or changed rules
✓ migrated to schema b032bbb55687b2c6
```

The migration, the changes to the tables and a commit recording the new schema happen in **one
transaction**: if anything fails, the database is exactly as before.

```bash
quadra db provisional
```

```text
  Product#32.origin: Ask the supplier (until 2027-01-31)
```

That's the sencha: someone has until the end of January to ask the supplier.

```bash
quadra db verify
```

```text
✓ hash chain intact, tables match the history (58 records)
```

::: tip In the browser
With `quadra serve --editors …`, pressing **Commit & deploy** in the code editor does the same
steps: check, tests, dry run on the live data, git commit, migration, reload.
:::

## A change: bulk data updates, approved

Raising coffee prices by 5% is not a change of the model, it's a change of the data. Running
an ad-hoc `UPDATE` in production is how mistakes happen: a wrong filter, no review, no record
of why. A `change` block is a data script with a reason, a requester and an approval.
Create `changes/coffee_prices.qd`:

```quadra
# A change is a data script with a reason, a requester and an approval.
# It can be previewed, is applied once, and can be reverted by a new transaction.

from catalog import Product
from security import Accountant

change coffee_prices_2027:
    reason: "Green coffee costs more: +5% on coffee, rounded to 10 cents"
    requested_by: "olivia"
    approved_by: role Accountant                # someone else, with this role, must approve

    for p in Product.where(p => p.category?.name == "Coffee" and p.active):
        p.price = (p.price * 1.05).round(step=0.10)

    expect:
        affected(Product) <= 10                 # a safety net against a wrong filter
```

The change runs as the user who requested it, with their permissions, and needs the approval
of **another** user with the role in `approved_by`.

```bash
quadra change list
```

```text
  coffee_prices_2027           pending
                               Green coffee costs more: +5% on coffee, rounded to 10 cents
```

Preview it — on the real data, without writing:

```bash
quadra change preview coffee_prices_2027
```

```text
coffee_prices_2027: Green coffee costs more: +5% on coffee, rounded to 10 cents
  approval: an Accountant
  ~ Product#31.price: 18.00 EUR/kg → 18.90 EUR/kg
✓ preview: constraints and expectations hold; nothing written
```

Apply it. Without an approver, with the requester as approver, or with someone without the
role, it is refused:

```bash
quadra change apply coffee_prices_2027
```

```text
error: this change needs the approval of an Accountant: pass --approved-by <login>
```

```bash
quadra change apply coffee_prices_2027 --approved-by olivia
```

```text
error: `olivia` is not an Accountant: the change needs the approval of an Accountant
```

```bash
quadra change apply coffee_prices_2027 --approved-by alex
```

```text
coffee_prices_2027: Green coffee costs more: +5% on coffee, rounded to 10 cents
  approval: an Accountant
  ~ Product#31.price: 18.00 EUR/kg → 18.90 EUR/kg
✓ applied in transaction 7
```

Every value it wrote has the change as its cause, in the history and in `why()`. A change is
applied only once:

```text
error: change `coffee_prices_2027` was already applied (transaction 7)
```

Changed your mind? Reverting writes a **new** transaction that restores the previous values —
refused if those values have changed again since, or if the change created records or wrote to
ledgers:

```bash
quadra change revert coffee_prices_2027 --approved-by alex
```

```text
✓ reverted coffee_prices_2027: 1 values restored in a new transaction
```

```bash
quadra change list
```

```text
  coffee_prices_2027           applied (tx 7), reverted (tx 8)
                               Green coffee costs more: +5% on coffee, rounded to 10 cents
```

## Tests

The step's project still passes all its tests with the new model (the migration itself runs
against your database, not in `quadra test`):

```bash
quadra test
```

```text
…
37 passed, 0 failed
```

## Try it

1. Add `origin` for `TEA-01` to `origins.csv` on a fresh database (`db init`, `db fixture demo`
   with the step 11 code, then the step 12 code and `db migrate`): no provisional values left.
2. Write a change that deactivates every product with no sales, with an `expect:` that stops
   it if it would touch more than 3 products.
3. Add a `Customer.segment: enum(Consumer, Cafe, Retailer)` with a migration that fills it from
   `kind` (`Person` → `Consumer`, `Business` → `Cafe`).

## Recap

- A `migration` travels with the code change: `rename` (history untouched), `convert` (every
  value through match arms), `backfill` (code, temporary entities from CSV), `verify`.
- `provisional(…)` keeps the schema valid while a value is still unknown, with a deadline;
  `quadra db provisional` lists them.
- `quadra db migrate --dry-run` checks the new model against the real data; `migrate` applies
  everything in one transaction.
- A `change` is a reviewed, approved data script: `preview`, `apply --approved-by`, `revert`.

[Next: talking to other systems →](./13-talking-to-other-systems)
