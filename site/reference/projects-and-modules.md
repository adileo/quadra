# Projects and modules

A Quadra project is a folder with a `quadra.toml` file. Every `.qd` file under it is a
module, and modules see each other only through explicit imports. Two folders are special:
`tests/` and `migrations/` hold scripts, which see the whole project without imports.

## Layout

```text
shop/
├── quadra.toml            marks the project root
├── core/
│   └── base.qd            module core.base
├── partners.qd            module partners
├── sales.qd               module sales
├── metrics.lock           written by `quadra certify`
├── customs.csv            data for a migration (`load from`)
├── migrations/
│   └── 2026-12-01_customs.qd   script: migrations.2026-12-01_customs
├── tests/
│   ├── fixtures.qd        script: tests.fixtures
│   └── sales.test.qd      script: tests.sales.test
└── .quadra/
    └── data.db            default SQLite database (created by `quadra db init`)
```

| Rule | Detail |
|---|---|
| Project root | The nearest folder, from the given path upwards, that contains a `quadra.toml` file. Every command accepts a path inside the project. |
| Source files | Every `*.qd` file below the root, at any depth. |
| Skipped folders | Folders whose name starts with `.`, `target`, `node_modules`, and any sub-folder that has its own `quadra.toml` (a nested project). |
| Encoding | UTF-8. See [Syntax](/reference/syntax). |

## `quadra.toml`

```toml
[project]
name = "shop"
edition = "2026"

[database]
url = "sqlite:data/shop.db"
```

| Key | Used for |
|---|---|
| `[project] name` | The name of the project. The tools do not read it yet: the file's presence is what marks the root. |
| `[project] edition` | The edition of the language the project is written in. Today there is one, `2026`, also the default when the key is missing. An edition this version of Quadra does not know stops every command with an error, rather than reading the code differently. |
| `[database] url` | The database of the `quadra db`, `quadra change`, `quadra token` and `quadra serve` commands, when `--url` is not given. A relative `sqlite:` path is resolved from the project root. |
| `[vars]` | The variables whose values depend on where the app runs, some of them secret: [Variables and connections](/reference/connections#variables). |
| `[connections.<name>]` | The systems the app sends to (HTTP, SMTP): [Variables and connections](/reference/connections#connections). |

When neither `--url` nor `[database] url` is given, the database is
`sqlite:<root>/.quadra/data.db`. Database URLs are described in [Storage](/reference/storage#database-urls).

## Modules

Each file is one module. Its name is its path relative to the root, without the extension,
with `/` replaced by `.`:

| File | Module |
|---|---|
| `sales.qd` | `sales` |
| `core/base.qd` | `core.base` |
| `tests/sales.test.qd` | `tests.sales.test` |

A module contains top-level declarations: `entity`, `record`, `trait`, `extend`, `enum`,
`ledger`, `fn`, `const`, `event`, `seed`, reactions (`on …`, `before …`), `role`,
`deny` / `allow` rules, `view`, `report`, `metric`, `control`, `migration`, `change`,
`fixture` and `test`. Each is described in its own page.

Two declarations with the same name in one module are an error:

```text
error: `Customer` is already declared at sales.qd:3
```

## Imports

```quadra
from core.base import Address, Document
from partners import Partner, PaymentTerm
```

`from <module> import <name>, …` makes declarations of another module visible by their
name. There is no `import module` form and no renaming; every name is imported explicitly.

| Situation | Message |
|---|---|
| A name used without importing it | `` unknown type `Customer`: it is declared in `sales`, add `from sales import Customer` `` |
| The module does not exist | `` unknown module `sale` `` |
| The name is not in that module | `` `Nope` is not declared in module `sales` `` |
| A module imports itself | ``a module cannot import itself`` |
| Two imports of the same name | warning: `` `Partner` is imported more than once `` |
| Cycles between modules | ``import cycle: billing → sales → billing (use `extend` in the higher module instead)`` |

Imports may not form cycles. Organize modules in layers (for example
`core` → `partners` → `products` → `sales` → `invoicing`). When a lower module's entity needs a
field that refers to a higher one, add it from the higher module with
[`extend`](/reference/entities#extend):

```quadra
# invoicing.qd: sales.qd cannot import Invoice, so invoicing adds the link itself
from sales import SalesOrder, SalesLine

extend SalesOrder:
    live invoices          # keeps following new invoices after the order is frozen
    invoices = lines.flat_map(l => l.invoice_lines).map(i => i.invoice).distinct()
```

## The standard library

Three modules are built into the compiler and are visible in every file without an import.
Their names are reserved: declaring an entity, enum or function with one of them is an error
(`` `Company` is declared in the standard library (`std.org`): use `extend Company:` ``); use
`extend` to add to their entities.

| Module | Declares |
|---|---|
| `std.money` | `enum Rounding: HalfEven, HalfUp, Up, Down` · `entity Currency` (`code`, `name`, `symbol`, `decimals` default 2, `rounding` default `HalfEven`: how `.round()` rounds its amounts, `active`) · `entity ExchangeRate` (`base`, `quote`, `date`, `rate`) · `fn exchange_rate(src: Currency, dst: Currency, on: Date) -> Decimal` · seeds for EUR, USD, GBP, CHF, JPY (0 decimals) and CNY |
| `std.uom` | `entity UomCategory` (`name`, `uoms`, `reference`) · `entity Uom` (`code`, `name`, `category`, `factor`, `precision` default 0.001) · seeds for `pcs`, `kg`, `g`, `t`, `l`, `ml`, `m`, `cm`, `h`, `min` |
| `std.org` | `entity Company` (`name`, `currency`) · `entity User` (`login`, `name`, `roles`) · `entity Sequence` (`company`, `code`, `pattern` default `"{code}/{year}/{n:05}"`, `gapless`) · `builtin fn next_number(company: Company, code: Code, on: Date = today()) -> Code` |

`next_number` gives the next number of a sequence, per company, code and year, filling the
pattern's `{code}`, `{year}` and `{n}` (with an optional [f-string format](/reference/syntax#f-strings):
`{n:05}`). Every sequence is gapless: the number is taken inside the transaction and kept
only if it commits, so a transaction that fails or is refused leaves no gap, and two
transactions that take a number of the same sequence at the same time are serialized (the
later one is refused as a conflict and retried). `gapless: true` states the requirement for
the readers of the model (invoices must have no gaps); `gapless: false` does not make the
engine skip numbers.

Projects add fields to these entities with `extend`, for example
`extend Company: vat_number: VatNumber unique`.

Currency and unit codes are names in every expression: `EUR` is the `Currency` record with
code `EUR`, `kg` is the `Uom` with code `kg`. The type checker learns which codes exist, and
which units belong to the same category, from the `seed` blocks (see
[Types](/reference/types#where-currencies-and-units-come-from)). To add a unit or a currency,
add a seed:

```quadra
seed:
    let packs = UomCategory(name="Packs")
    Uom(code="box", name="Box", category=packs, factor=1, precision=1)
    Currency(code="SEK", name="Swedish krona", symbol="kr")
```

`Money`, `Price`, `Qty`, `Exact`, `convert()`, `allocate()` and the other built-in
functions are part of the language itself, not of the standard library; see
[Types](/reference/types) and [Expressions](/reference/expressions#built-in-functions).

## Scripts: `tests/` and `migrations/`

A `.qd` file inside a top-level `tests/` or `migrations/` folder is a **script**. Scripts
differ from modules in two ways:

| | Modules | Scripts |
|---|---|---|
| Sees declarations of other modules | only what it imports | every declaration of every module, without imports |
| Can be imported by modules | yes | no: `` `tests.fixtures` is a script (tests, migrations): modules cannot import from it `` |
| Can import from other scripts | — | yes (`from tests.fixtures import italy`) |

Scripts usually contain `test`, `fixture`, `migration`, `change` and `temporary entity`
declarations, but these can be written in any file: a small project can keep its tests next
to its entities. A file directly in the root named `tests.qd` is an ordinary module.

```quadra
# tests/order_to_cash.test.qd
from tests.fixtures import italy

test "an order is invoiced":
    use italy
    let so = SalesOrder(customer=rossi, lines=[SalesLine(product=widget, qty=3 pcs)])
    expect so.total == 36.60 EUR
```

Here `SalesOrder` and `SalesLine` need no import (scripts see all modules), but the fixture
`italy`, declared in another script, does.

## Naming

The compiler does not enforce a naming style, but it relies on one convention:

| Kind | Style | Examples |
|---|---|---|
| Entities, records, traits, enums, ledgers, roles, events | `PascalCase` | `SalesOrder`, `Stock`, `SendEmail` |
| Enum variants and flow states | `PascalCase` | `Draft`, `Confirmed`, `OneProduct` |
| Fields, formulas, actions, functions, metrics, fixtures | `snake_case` | `credit_limit`, `create_invoice`, `italy` |
| Unit and currency codes | as seeded | `EUR`, `pcs`, `kg` |

In a [`match`](/reference/expressions#match) pattern, a name that starts with an upper-case
letter is a variant (or an entity type); a lower-case name binds the value. Name your
variants with a capital letter.

Most keywords are contextual, so ordinary words remain available as field names (`key`,
`parent`, `by`, `check`…). The reserved words are listed in [Syntax](/reference/syntax#keywords).
