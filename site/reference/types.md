# Types

Quadra's types know about money and measures: an amount carries its currency, a quantity its
unit, a price both. The type checker refuses what makes no sense in accounting (adding euros
to dollars, kilograms to pieces, storing an unrounded amount), and the engine converts what
can be converted exactly.

## All types

| Type | Values | Notes |
|---|---|---|
| `Text` | `"Milano"` | Unicode text. |
| `Code`, `Email`, `Phone`, `VatNumber`, `Barcode` | text | Behave exactly like `Text`; the name documents the intent and guides the user interface. No format is validated: add a `where` for that. |
| `File`, `File<pdf, image>` | `sha256:<hex>` | A file uploaded or rendered by the app: its bytes are in the store of files, the field holds their SHA-256. See [Files](#files). |
| `Int` | `42` | Whole numbers. |
| `Decimal` | `0.92` | Exact decimal numbers (never floating point). |
| `Percent` | `22%` | A number written as a percentage: `22%` is 0.22. |
| `Bool` | `true`, `false` | |
| `Date` | `2026-11-21` | A day of the calendar, in no zone. |
| `DateTime` | `2026-11-21 09:00` | An instant (kept in UTC); written in the code, it is a time of the project's clock. See [Dates, times and time zones](#dates-times-and-time-zones). |
| `Duration` | `8 hours`, `30 minutes`, `30 days`, `2 weeks` | Also the difference of two dates or date-times. Shown as `8 h`, `1 h 30 min`, `3 days`. |
| `DateRange` | `DateRange(2026-11-01, 2026-11-30)` | Both ends included. |
| `Money<C>` | `10.50 EUR` | An amount in currency `C`. |
| `Price<C, U>` | `9.90 EUR/pcs` | An amount in `C` per unit `U`. |
| `Qty<U>` | `3 pcs` | A quantity in unit `U`. |
| `Exact<T>` | | A `Money`, `Price` or `Qty` computed but not yet rounded. |
| `T?` | a `T` or `none` | Optional. |
| `[T]` | `[1, 2, 3]` | A list (ordered, duplicates allowed). |
| `{T}` | `{Receivable, Payable}` | A set. |
| `(name: T, …)` | `(due: d, amount: a)` | A named tuple. |
| `enum(A, B)` / an `enum` name | `Draft` | A closed set of values; see [Entities](/reference/entities#enum). |
| an entity, record or trait name | a record | A reference to a record (or, for a `record`, the value itself). |
| `Role` | `Admin` | A role declared with `role`; `User.roles` is a `{Role}`. |

`date.month` has a year-month type (written `2026-11` as a literal); it has no name you can
declare.

There are no anonymous unions. `x: Int | Text` is refused:
``anonymous unions are not allowed: use `enum(A, B)` for a set of values, or a trait for references to different entities``.

## Files

A `File` field holds a file: a scan, a photo, a contract. Its bytes are never in the
database: they are in the store of files (a folder, or an S3 bucket: [`[files]`](/reference/projects-and-modules#files)),
and the field holds `sha256:<hex>`, the SHA-256 of the bytes. The same file uploaded twice is
kept once.

```quadra
entity Expense:
    receipt: File<pdf, image>? max 10 MB
    photos: [File<image>] max 2 MB
    contract: File<word>?
    archive: File<"application/zip">?
    anything: File?
```

The type's arguments say which files the field takes: families, or an exact type in quotes.
`File` without arguments takes every file.

| Family | Types |
|---|---|
| `pdf` | PDF |
| `image` | JPEG, PNG, WebP, GIF, HEIC (never SVG: it can run scripts) |
| `word` | DOCX, DOC, ODT, RTF |
| `excel` | XLSX, XLS, ODS, CSV |
| `text` | plain text, Markdown, CSV |
| `zip` | ZIP |
| `video` | MP4, QuickTime, WebM, Matroska, AVI, MPEG, 3GP |
| `audio` | MP3, WAV, Ogg, FLAC, M4A, AAC |

An unknown family is an error that lists them:
``` `pdff` is not a family of files: one of pdf, image, word, excel, text, zip, video, audio, or an exact type in quotes ("application/zip") ```.

`max 10 MB` (also `B`, `KB`, `GB`, powers of 1024) is the largest file the field takes; on a
list it is the largest of each file. Without `max`, the largest is 100 MB; `max` goes up to
5 GB. `max` on a field that is not a `File` is an error.

The server tells the type of a file from its bytes, never from its name or from what the
browser says: a program renamed `report.pdf` is still a program, and a field that takes
`pdf, image` refuses it
(`report.pdf: the file is an application/x-msdownload, but Expense.receipt accepts pdf, image`).
Office documents are told apart by what is inside their zip (`word/`, `xl/`, the `mimetype`
of OpenDocument). Plain text cannot be told apart by its bytes: a text file is `text/csv` when
its name ends in `.csv`, `text/markdown` with `.md`, `text/plain` otherwise; HTML and SVG are
recognized by their content.

A field of the app's own documents ([`attach`](/reference/documents#attach-kept-with-the-record))
is a `File?` too; if it has arguments, they must take `pdf`. How files are uploaded and
downloaded: [Files](/reference/entities#files) and the [HTTP API](/reference/http-api#files).

## Dates, times and time zones

Two kinds of time, never mixed up:

- a **`DateTime` is an instant**: `now()`, the moment a record was created, when something
  happened. It is kept in UTC, so two of them compare without ambiguity, between projects and
  with other systems;
- a **`Date` is a day of the calendar**, in no zone: a due date, a birthday, a holiday.

The **project's zone** turns one into the other. Set it in `quadra.toml` (an IANA name; without
it, `UTC`):

```toml
[project]
name = "shop"
timezone = "Europe/Rome"
```

It is used only where an instant becomes a date or an hour, and back:

| | What it gives, in the project's zone |
|---|---|
| `today()` | the date now |
| `dt.date`, `dt.hour`, `dt.minute`, `dt.weekday` (1 Monday … 7 Sunday), `dt.month`, `dt.year` | the day, hour… of the instant |
| `datetime(due, "09:00")` | the instant of 9:00 on that day |
| `2026-11-24T09:00` written in the code | 9:00 on the project's clock |
| `dt + 1 day`, `dt + 2 weeks` | the same time of day, days of the calendar later |

Another zone is always written out, also taken from a record (`customer.timezone`):

```quadra
entity Reminder:
    customer: Customer
    at: DateTime
    local_day = at.in(customer.timezone).date      # the day where the customer is
    local_hour = at.in(customer.timezone).hour

today("America/New_York")                          # the date there now
datetime(due, "09:00", "Asia/Tokyo")               # 9:00 in Tokyo, as an instant
now().in("Europe/Rome").offset                     # 1 hour in winter, 2 hours in summer
```

`dt.in(tz)` is a `ZonedTime` (standard library): `instant`, `timezone`, `date`, `hour`,
`minute`, `weekday`, `offset`. Zone names are checked: a name written in the code that is not a
zone is an error, one from a field fails when it is used.

**Days and hours are not the same when the clocks change.** `+ 1 day` (and `+ N days`,
`+ N weeks`, written out) moves to the same time of the next day in the project's zone;
`+ 24 hours` moves 24 exact hours. On the night the clocks go back (25 hours long in Rome):

```quadra
let nine = datetime(2026-10-24, "09:00")
nine + 1 day        # 2026-10-25 09:00
nine + 24 hours     # 2026-10-25 08:00
```

A time that does not exist (the hour skipped when the clocks go forward: 02:30 on 29 March 2026
in Rome) moves forward by the gap, to 03:30; a time that happens twice (02:30 on 25 October)
is the first of the two.

In tests, `clock 2026-11-24 09:30` sets the project's clock (a date alone: 9:00).

Through the HTTP API, instants always come out in ISO 8601 with their zone
(`2026-11-24T08:30:00Z`) and go in with any zone (`2026-11-24T09:30:00+01:00`); a time
without a zone is a time of the project's clock. See [HTTP API](/reference/http-api).

## Currencies and units in types

The argument of `Money`, `Price` and `Qty` is a currency or unit code, or a **path** to the
currency or unit of the record:

| Declaration | Meaning |
|---|---|
| `Money<EUR>` | always euros |
| `Qty<pcs>` | always pieces |
| `Money<currency>` | the currency stored in the field `currency` of the same record |
| `Money<order.currency>` | the currency of the record's order |
| `Qty<product.uom>` | the unit of the record's product |
| `Price<order.currency, uom>` | per unit `uom` of the same record, in the order's currency |
| `Money<company.currency>` | in a `per company` field: the currency of the company being computed; elsewhere, the field `company` of the record |
| `Money<owner.currency>` | in a `record` type: the currency of the entity that holds the record |
| `Money<current.company.currency>` | the currency of the session's company |
| `Money<amount.currency>` | in a function's return type: the currency of the parameter `amount` |
| `Money` | any currency (in a parameter: the currency of the argument) |

Path-based currencies make the type checker's job harder, but they model multi-currency
data faithfully: each sales order in its own currency, each company in its own.

### Where currencies and units come from

Currencies and units are records (`Currency`, `Uom`), seeded by the standard library and by
your own `seed` blocks. The type checker reads the seeds:

- `Currency(code="SEK", …)` declares the currency `SEK`;
- `Uom(code="box", category=packs, …)` declares the unit `box`, in the category created by
  `let packs = UomCategory(name="Packs")` in the same `seed` block.

Units of the same category can be converted into each other, through their `factor` (the
size of the unit in the category's reference unit). Units of different categories cannot be
mixed.

| Category (standard library) | Units (factor) |
|---|---|
| Units | `pcs` (1) |
| Weight | `kg` (1), `g` (0.001), `t` (1000) |
| Volume | `l` (1), `ml` (0.001) |
| Length | `m` (1), `cm` (0.01) |
| Time | `h` (1), `min` (1/60) |

`minute(s)`, `hour(s)`, `day(s)` and `week(s)` are not units: they make a `Duration`. `(90 minutes).to(h)` is `1.5 h`.

## Arithmetic

### Adding, subtracting, comparing

| Operands | Result | When refused |
|---|---|---|
| `Money<A> ± Money<B>` | `Money<A>` | `A` and `B` are different currencies: `cannot add Money<EUR> and Money<USD>` |
| `Qty<A> ± Qty<B>` | `Qty<A>` (the right side is converted to `A`) | different categories: `cannot add Qty<kg> and Qty<pcs>` |
| `Price<A, U> ± Price<B, V>` | `Price<A, U>` | different currencies |
| amount, quantity or price `± 0` | the same type | — |
| amount, quantity or price `±` another number | — | always: `cannot add Money<EUR> and Int` |
| `Date ± Duration`, `DateTime ± Duration` | the same type | — |
| `Date - Date`, `DateTime - DateTime` | `Duration` | — |
| `Int ± Int` | `Int` | — |
| `Percent ± Percent` | `Percent` | — |
| other numbers | `Decimal` (`1 - 10%` is 0.9) | — |
| `Text + Text` | `Text` | — |
| `[T] + [T]`, `{T} + {T}` | concatenation, union | — |

Comparisons (`== != < <= > >=`) follow the same rules: `2 kg > 500 g` is true, `10.00 EUR > 5`
is refused (`cannot compare Money<EUR> and Int`), `balance > 0` and `price > 0` are allowed.

`0` stands for the zero of the type it meets: `cost: Money<EUR> default 0` stores `0.00 EUR`,
`price: Price<EUR, pcs> default 0` stores `0.00 EUR/pcs`, and the sum of no amounts is `0.00 EUR`
(the sum's currency or unit, also when it is a path like `order.currency`).

### Multiplying and dividing

| Operation | Result |
|---|---|
| `Qty<U> × Price<C, V>` | `Exact<Money<C>>`; `U` and `V` must be of the same category (`10 min × 60.00 EUR/h` = 10.00 EUR) |
| `Money × Int` | `Money` (a product by an integer needs no rounding) |
| `Money × Decimal` or `Percent` | `Exact<Money>` |
| `Qty × Int` | `Qty` |
| `Qty × Decimal` or `Percent` | `Exact<Qty>` |
| `Price × Int` | `Price` |
| `Price × Decimal` or `Percent` | `Exact<Price>` |
| `Duration × number` | `Duration` |
| `Money / Money`, `Qty / Qty`, `Price / Price` | `Decimal` (quantities are converted first) |
| `Money<C> / Qty<U>` | `Exact<Price<C, U>>` |
| `Money`, `Price` or `Qty` `/ number` | `Exact<…>` of the same type |
| `number / number` | `Decimal` (`7 / 2` is 3.5) |

Refused: `Money × Money`, `Money × Qty`, and `Qty × Price` across categories
(`cannot multiply Qty<kg> by Price<EUR, pcs>`). Dividing by zero fails at run time with
`division by zero`.

## Rounding and `Exact`

An amount, price or quantity that comes out of a multiplication or a division is
`Exact<…>`: it keeps every decimal. It must be rounded before it is stored in a field, as a
default, a typed formula, an assignment or a constructor argument:

```quadra
amount = (qty * price * (1 - discount)).round()              # ok: inferred Money<EUR>
net_price: Price<order.currency, uom> default price * (1 - discount)
# error: Exact<Price<order.currency, uom>> must be rounded with .round() before it is stored as Price<order.currency, uom>
```

| Method | Rounds to |
|---|---|
| `money.round()` | the currency's `decimals` (2 by default, 0 for JPY) |
| `money.round(step=0.05)` | the nearest multiple of `step` |
| `price.round()` | the currency's decimals plus 2 (`1.23456 EUR/pcs` → `1.2346 EUR/pcs`) |
| `qty.round()` | the unit's `precision` (0.001 by default, 1 for `pcs`) |
| `qty.round_up()` | the unit's `precision`, always upwards |
| `decimal.round()` | 2 decimals |

Ties are rounded to the even neighbour (banker's rounding): `10.005 EUR` → `10.00 EUR`,
`10.015 EUR` → `10.02 EUR`, `2.5 pcs` → `2 pcs`. Amounts and prices follow the `rounding` of
their currency (`Currency.rounding`, `HalfEven` by default):

| `rounding` | `10.005 EUR` | `-10.005 EUR` | `10.001 EUR` |
|---|---|---|---|
| `HalfEven` | `10.00 EUR` | `-10.00 EUR` | `10.00 EUR` |
| `HalfUp` (ties away from zero) | `10.01 EUR` | `-10.01 EUR` | `10.00 EUR` |
| `Up` (away from zero) | `10.01 EUR` | `-10.01 EUR` | `10.01 EUR` |
| `Down` (towards zero) | `10.00 EUR` | `-10.00 EUR` | `10.00 EUR` |

`round(step=…)` rounds to the multiple of the step with the same mode; quantities and plain
decimals always round half-even.

To split an amount into parts that add up exactly, use
[`allocate`](/reference/expressions#allocate) instead of rounding each part.

An untyped formula keeps its exact type: `avg = total / count` is `Exact<Money>`. Declare
`Exact<T>` when you want to keep the precision on purpose:

```quadra
avg_cost: Exact<Price<company.currency, uom>> per company =
    stock_value / stock_qty if stock_qty > 0 else standard_cost
```

## Conversions

| Expression | Result |
|---|---|
| `qty.to(kg)` | the quantity in another unit of the same category; across categories: `cannot convert Qty<pcs> to kg: different kinds of unit` |
| `price.to(g)` | the price per another unit (`10.00 EUR/kg.to(g)` = `0.01 EUR/g`) |
| `duration.to(h)` | the duration as a quantity of a time unit (`2 days.to(h)` = `48 h`) |
| `convert(x, to=EUR, on=date)` | an amount or price in another currency, at the rate of `on`; `Exact` |
| `convert(x, to=EUR, rate=0.92)` | the same, at a given rate |
| `convert(x, to=EUR)` | at today's rate (not allowed in a formula) |
| `Qty(3, product.uom)` | a quantity in a unit known only at run time |
| `Price(amount, per=uom)` | a price from an amount and a unit |

`convert` reads the rate with the standard library's `exchange_rate(src, dst, on)`: the
`ExchangeRate` with the latest `date` on or before `on`, in either direction (the inverse
rate is `1 / rate`), and 1 for the same currency. Without a rate:
`Missing exchange rate GBP/EUR on 2026-11-25`.

In a formula, `convert` needs `on=` or `rate=`:
``convert() in a formula needs `on=` or `rate=`: the exchange rate of today is not deterministic``.

`.raw` drops the unit: `3.25 EUR.raw` is `3.25`. Use it to combine values of different
currencies or units on purpose, for example as weights:
`moves.allocate(amount, by=m => m.qty_base.raw)`.

## Sums over records

Summing amounts over records is allowed only if they are in one currency:

```quadra
untaxed = lines.sum(l => l.subtotal)            # ok: every line is in the order's currency
all = SalesLine.all().sum(l => l.subtotal)      # error
```

```text
error: cannot sum Money<l.order.currency>: amounts in different currencies (each `l` has its own); convert them first
```

### How the checker proves two currencies equal

Two path-based currencies are the same when the checker can show the paths lead to the same
record. It uses what the code states:

| Source | Example | What it learns |
|---|---|---|
| Inverse relations | `lines.sum(l => l.subtotal)` in `SalesOrder` | each `l.order` is `self` |
| `where` filters | `GL.where(g => g.company == company).sum(g => g.amount)` | each `g.company` is `company` |
| `where` on reference fields | `invoice: Invoice? where i => i.currency == payment.currency` | `invoice.currency` is `payment.currency` |
| Rows of a ledger group | `check per (company): g => g.sum(r => r.amount) == 0` | each row's `company` is the group's |

When it cannot decide (a currency known only at run time), the checker trusts the code and
the engine checks the values: `cannot add 10.00 EUR and 5.00 USD: different currencies`.

## Optional values

A value of type `T?` may be `none`. Reading a field or calling a method on it needs `?.`,
unless the checker knows it is not `none`:

```text
error: `customer` is optional: use `?.` (or check it is not none first)
```

| Form | Meaning |
|---|---|
| `x?.field`, `x?.method()` | `none` if `x` is `none` |
| `x ?? y` | `x`, or `y` if `x` is `none` |
| `x != none and x.field > 0` | after `x != none and`, `x` is known not to be `none` |
| `x == none or x.field > 0` | the same after `x == none or` |
| `if x != none:` … | inside the block |
| `if x == none: return …` | after the `if` (also with `fail`) |
| `if let v = x:` … | `v` is the value of `x`, when it is not `none` |
| `x?.kind is Kit` | when true, `x` is not `none` |
| `field: T? required when cond` | the field is read as `T` |

Inside an f-string, and in the messages of `check` and `require`, optional values can be read
without `?.`.

## Kinds of value

A value of one kind cannot be stored where another kind is expected, even in a default:

```text
error: cannot store Qty<pcs> in a default of type Qty<kg>
error: cannot store Text in a default of type Date
error: expected Customer, found Order
```

When the currency or unit is a path known only at run time (`min_order: Qty<uom>?`), the
value is checked when the record is saved: another currency, or a unit of another kind, is
refused (`Product.min_order = 2 kg: expected a quantity in pcs`); another unit of the same
kind is accepted (`500 g` where `uom` is `kg`).

## Inference

Formulas (`name = expr`), `let` variables and lambdas have inferred types; fields with
`default`, parameters and return types are declared.

| Expression | Inferred type |
|---|---|
| `0` | compatible with any number, amount, quantity or price |
| `42` / `4.2` / `42%` | `Int` / `Decimal` / `Percent` |
| `lines.sum(l => l.amount)` | the type of `l.amount` |
| `a if c else b` | the type of `a` and `b`; with `none` on one side, optional |
| `x ?? y` | the type of `y`, not optional |
| `when:` / `match` | the type of the arms |
| `lines.first()` | the element type, optional |

The checker is **gradual**: it reports what it can prove, and when a type is not known
statically (an untyped parameter, a value returned by a trait member) it trusts the code. The
engine checks every value at run time.
