# Migrations and changes

Data outlives code. When the model changes, a **migration** brings the existing data to the
new model; when data must be changed in bulk, a **change** does it with a reason, a requester
and an approval. Both are written in Quadra, both can be previewed, and both are recorded in
the signed history as the cause of every value they write.

| | Migration | Change |
|---|---|---|
| When | the model changed (a field renamed, a type changed, a new required field) | the data must change, the model does not |
| Runs | once, with `quadra db migrate` (or a deploy from the web editor) | once, with `quadra change apply` |
| Preview | `quadra db migrate --dry-run` | `quadra change preview` |
| Approval | the deploy itself | `approved_by: role X`, a different user than the requester |
| Undo | a new migration | `quadra change revert` (a new transaction) |

## Migrations

```quadra
# migrations/2026-12-01_customs.qd
migration customs:
    reason: "Customs codes are required for shipping outside the EU"

    rename Product.product_kind -> Product.type
    convert Product.type from old.Text:
        "merce"    => Storable
        "consumo"  => Consumable
        "servizio" => Service
        other      => fail f"Unknown product kind: {other}"

    backfill Product.hs_code:
        for p in Product.all():
            if let code = CustomsCode.find(c => c.product_code == p.code)?.code:
                p.hs_code = code
            else:
                p.hs_code = provisional("00000000", reason="To classify", until=2027-01-31)

    verify:
        Product.count(p => p.hs_code.is_provisional) < 2


temporary entity CustomsCode:
    product_code: Text
    code: Code where c => c.len() == 8
    load from "customs.csv"
```

This migration accompanies a change of the model, in the same commit. Before:

```quadra
entity Product:
    code: Code unique
    name: Text
    product_kind: Text
    list_price: Money<EUR>
```

After:

```quadra
enum ProductType: Storable, Consumable, Service

entity Product:
    code: Code unique
    name: Text
    type: ProductType
    hs_code: Code where c => c.len() == 8
    list_price: Money<EUR>
```

`migration name:` is followed by steps, which run in order:

| Step | Effect |
|---|---|
| `reason: "…"` | why the migration exists |
| `rename Entity.old -> Entity.new` | the field's values (and its history) are now read under the new name |
| `convert Entity.field from old.Type:` + arms | every stored value is replaced by the value of the first arm that matches it |
| `backfill Entity.field:` + block | runs the block, which assigns the field on existing records |
| `verify:` + block | every expression line of the block must be true |
| any other statement | runs as in an action |

### `rename`

The history is never rewritten: `rename` records that, from this transaction on, the field
`old` is called `new`. Values written before are read under the new name, and the signatures
of past commits stay valid.

### `convert`

The arms are [`match`](/reference/expressions#match) arms over the old value: literals,
`_`, or a lower-case name that binds it (`other`). Every record is converted; the values
that match no arm, or whose arm fails, are collected and reported together (at most 20):

```text
error: the existing data does not satisfy the new model: migration customs: convert Product.type: 1 values cannot be converted:
  Product#26: Unknown product kind: altro
```

### Provisional values

A required value that nobody knows yet can be filled with
`provisional(value, reason="…", until=date)`:

- the value satisfies the field's type and rules, so the migration can complete;
- it is marked, and `record.field.is_provisional` is true;
- `quadra db provisional` lists every provisional value with its reason and deadline;
- writing a real value removes the mark;
- after `until`, any transaction that changes that record is refused until the value is
  replaced:
  `Product#26.hs_code is provisional (To classify) and expired on 2027-01-31: give it a real value`.

### Staging data: temporary entities

A [`temporary entity`](/reference/entities#temporary-entities) holds the data a migration
needs, typically loaded from a CSV file with `load from`. Its records are created when the
migration starts and removed before it commits: the database never has a table for it.

### How a migration is applied

```bash
quadra db status            # schema fingerprint CHANGED, 1 pending migration
quadra db migrate --dry-run # everything below, then nothing is written
quadra db migrate
```

```text
  migration customs
    rename Product.product_kind → Product.type: 3 values (the history keeps the old name)
    convert Product.type: 3 values
    backfill Product.hs_code: 3 values, 1 provisional
    verify: ok
  rename column product.product_kind → type
  copy table product
  every Product: 3 new or changed rules
✓ migrated to schema 640eaafd35d6548c
```

`quadra db migrate` does everything in one transaction:

1. runs the pending migrations (those not yet applied to this database), in order;
2. gives new fields that have a `default` their default on every existing record that lacks
   it;
3. checks, on every existing record, the rules the new code adds or changes (required values,
   `where`, `check`, `unique`, ledger checks, `frozen when`), and every rule that reads a value
   step 1 or 2 wrote; a rule that did not change, on data that did not change, still holds;
4. changes the tables that changed, and only them; computes again the formulas whose
   definition changed; and commits.

If any step fails, nothing changes. A model change without the migration it needs is
refused by step 3:

```text
error: the existing data does not satisfy the new model: Product.type is required; Product.hs_code is required
```

### What a deploy changes in the database

The database keeps, next to the data, what the code deployed last looks like: its tables,
its rules and formulas (`_quadra_meta`, key `catalog`). A deploy compares the new code with
it and does the least the difference needs, whatever the size of the data:

| Change | In the database |
|---|---|
| nothing that touches the data (code of actions, views, permissions) | the deploy's commit, nothing else |
| a new field | `ALTER TABLE … ADD COLUMN` |
| a new field with a `default` | a new column; the default is computed for every existing record and written to the history (in batches: memory does not grow with the data) |
| a field removed | `DROP COLUMN` (the history keeps its values) |
| `rename E.old -> E.new` in a migration | `RENAME COLUMN` (and its join table and indexes follow it) |
| a field removed and another added, without a `rename` | a column dropped, another added: the values do not follow (a required new field is refused: `E.new is required`) |
| a field whose type changes | its column is dropped and filled again from the history |
| a new entity, a removed one | `CREATE TABLE`, `DROP TABLE` |
| a new or changed `check`, `where`, `required`, `unique`, `frozen when` | checked on every record of the entity, nothing else |
| new or changed rules only (nothing else written by the deploy) | in SQL when they translate (see [Loading on demand](/reference/storage#loading-on-demand)): a `check`, a `required` or `required when`, the `where` of a field: one query per rule, `SELECT … WHERE NOT (rule)`; a `unique` without `when`: the records whose key is there more than once (`GROUP BY … HAVING COUNT(*) > 1`), a page at a time. Only those records are loaded, and checked by the engine, which reports them as below; new rules and new `unique`s together, a `warn`, a `unique … when` are checked on every record |
| a formula whose definition changes | computed again on every record of the entity, in batches; the catalog records whether it is incremental ([Formulas kept up to date](/reference/storage#formulas-kept-up-to-date)) |

A change that `ALTER TABLE` cannot make on SQLite (a new `NOT NULL` or `UNIQUE` constraint, a
constraint on a column that changes) copies that table, in SQL, inside the transaction.
A database created by an older version of Quadra has no catalog: its first deploy rebuilds
every table from the history, as every deploy did before, and writes the catalog.

The tables have the indexes the model needs: on every reference (`parent` fields included),
on the far side of every many-to-many set, on the fields of a `unique … when`, on the fields
compared with `==` in row permissions. They are named `qx_<table>_<columns>` and follow the
model at every deploy: created, renamed, dropped. On SQLite they are built in the deploy's
transaction; on PostgreSQL, those of tables that already existed are built after the commit
with `CREATE INDEX CONCURRENTLY`, without blocking the application, and an interrupted build
is resumed by the next deploy (or by `quadra serve` when it starts).

### When existing data breaks a new rule

A new `check` that some records already break is refused with the rule, where it is
declared, every record that breaks it (the first five) and the values the rule reads:

```text
a rule of the new code is broken by data you already have:

  main.qd:11
    check date <= today(), "Date must be today or in the past"
  2 records of TimeLog break it:
    TimeLog#24: date = 2026-11-24
    TimeLog#25: date = 2026-12-01

The rule cannot hold on this data. Either:
  - correct those records (in the app, or with a `change` for many of them);
  - add a migration that fixes them: it runs before the rules are checked;
  - or change the rule.
```

The migration is the way when the fix is a rule of its own, the same for every record:

```quadra
# migrations/2026-11-22_no_future_time_logs.qd
from main import TimeLog

migration no_future_time_logs:
    reason: "Time logs cannot be dated in the future: those already entered move to today"

    for t in TimeLog.all().where(t => t.date > today()):
        t.date = today()

    verify:
        TimeLog.count(t => t.date > today()) == 0
```

Under `quadra dev`, correcting the records in the app is enough: after each transaction the
saved code is tried again, and applied as soon as the data fits it.

A new database (`quadra db init`) is created with the current model, so the migrations that
already exist count as applied. Each migration runs once per database.

Saving and committing from the web editor runs the same steps as a dry run before the git
commit, and for real after it (see [HTTP API](/reference/http-api#the-code-of-the-project)).

## Changes

```quadra
change raise_prices:
    reason: "Price list 2027: +10%"
    requested_by: "mario"
    approved_by: role Manager
    effective: 2027-01-01

    for p in Product.all():
        p.list_price = (p.list_price * 1.10).round()

    expect:
        affected(Product) <= 3
```

`change name:` is a data script with metadata:

| Line | Meaning |
|---|---|
| `reason: "…"` | why; part of the cause of every value written |
| `requested_by: "login"` | the user who asks for it: the change runs with their permissions |
| `approved_by: role Name` | the change needs the approval of a user who has that role, directly or through a role that inherits it (`role Director is Manager`), other than the requester |
| `effective: date` | it cannot be applied before that date |
| statements | the change itself |
| `expect:` + block | every expression line must be true at the end, or the change fails (`expect failed: affected(Product) <= 3`) |

`affected(Entity)` is the number of records of the entity created or changed by the change:
a safety net against a filter that is too broad.

### Preview, apply, revert

```bash
quadra change list
quadra change preview raise_prices
quadra change apply raise_prices --approved-by lucia
quadra change revert raise_prices --approved-by lucia
```

```text
raise_prices: Price list 2027: +10%
  approval: a Manager
  ~ Product#24.list_price: 1.00 EUR → 1.10 EUR
  ~ Product#25.list_price: 2.50 EUR → 2.75 EUR
  ~ Product#26.list_price: 40.00 EUR → 44.00 EUR
✓ preview: constraints and expectations hold; nothing written
```

| Command | Effect |
|---|---|
| `preview` | runs the change in a transaction that is never written: values before and after (the first 50), records created, ledger rows, the [certified metrics](/reference/metrics) it touches, and whether every rule and expectation holds |
| `apply` | runs it for real, in one transaction, as the requester; the cause of every value is `change raise_prices: Price list 2027: +10%, requested by mario, approved by lucia` |
| `revert` | a new transaction that puts back the values the change replaced |
| `list` | every change of the project: `pending`, `applied (tx 3)`, or `applied (tx 3), reverted (tx 4)` |

| Refused | Message |
|---|---|
| no approver | `this change needs the approval of a Manager: pass --approved-by <login>` |
| the approver lacks the role | `` `mario` is not a Manager: the change needs the approval of a Manager `` |
| the approver is the requester | `` `mario` requested the change and cannot approve it too `` |
| the requester is not a user | `` the requester `x` is not a user `` |
| before `effective` | `` change `raise_prices` is effective from 2027-01-01: it can be applied on or after that date `` |
| applied twice | `` change `raise_prices` was already applied (transaction 3) `` |

A revert needs the same approval, and is refused when it cannot be exact: when the change
created records or posted to ledgers, or when a value it wrote has changed again since (the
message lists them). Write a new change in those cases. A change can be reverted once.

### Correcting posted documents

A change runs the same code as the application, with the same rules: it cannot assign fields
of a frozen record. It can set the field of a flow (which application code cannot): setting it
back to a state where `frozen when` is false unfreezes the record. Corrections of posted
documents go through the actions of the process:

```quadra
change fix_wrong_vat_november:
    reason: "November invoices issued with 10% VAT instead of 22% (ticket 1432)"
    requested_by: "a.gialli"
    approved_by: role Accountant

    let vat22 = Tax.single(t => t.scope is Sale and t.rate == 22%)
    for inv in Invoice.where(i => i.direction is Out and i.state is Posted
                                  and i.date.month == 2026-11 and i.lines.any(l => l.tax.rate == 10%)):
        let credit = inv.credit_note(reason="Wrong VAT")
        credit.post()
        let fixed = inv.copy(lines=inv.lines.map(l => l.copy(tax=vat22)))
        fixed.post()
```
