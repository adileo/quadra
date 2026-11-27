# Expressions

Expressions compute values in formulas, defaults, rules, actions, tests and in the API's
filters. They have no side effects, except in action, reaction and test bodies, where
statements may create and change records.

## Precedence

From the loosest to the tightest binding:

| Level | Form |
|---|---|
| 1 | `as user: expr` · `fail expr` |
| 2 | `a if cond else b` |
| 3 | `a ?? b` (right-associative) |
| 4 | `a implies b` |
| 5 | `a or b` |
| 6 | `a and b` |
| 7 | `not a` |
| 8 | `== != < <= > >=` · `in` · `not in` · `is` · `is not` |
| 9 | `a .. b` |
| 10 | `+ -` |
| 11 | `* /` |
| 12 | unary `-` |
| 13 | `.name` · `?.name` · `(args)` · `[index]` |
| 14 | literals, names, `self`, `( … )`, `[ … ]`, `{ … }`, `when:`, `match`, lambdas |

Unary minus binds more loosely than a method call: `-3.00 EUR.abs()` is `-(3.00 EUR.abs())`.
Write `(-3.00 EUR).abs()`.

## Names

A bare name is looked up in this order:

| Name | Meaning |
|---|---|
| a local | a `let` variable, a parameter, a lambda parameter, a `match` binding |
| a field, formula, action or method of the current record | `total`, `lines`, `confirm` (inside an entity) |
| `self` | the current record |
| `current` | the session: `current.user`, `current.company` |
| `me` | the current user (`current.user`) |
| an entity name | all the records of the entity: `Product.where(p => p.active)` (on a database, answered in SQL: [Storage](./storage.md#loading-on-demand)) |
| an enum variant | `Draft`, `Storable` |
| a constant, a function, a metric | `STANDARD_VAT`, `price_for(…)`, `revenue(…)` |
| a currency or unit code | the `Currency` or `Uom` record: `EUR`, `kg` |
| a role | `Admin` |
| `env` | the variables of `quadra.toml`: `env.SHOP_NAME`, in actions, reactions and scripts only ([Variables and connections](/reference/connections#env-name)) |

Names always refer to locals or fields of the current record; there is no implicit scope.
Inside a lambda, use the parameter: `lines.where(l => l.qty > 0 pcs)`. A bare name in the
lambda body is still the current record's field: `moves.where(m => m.product == product)`
compares each move's product with the record's own `product`.

## Operators

| Operator | Meaning |
|---|---|
| `+ - * /` | arithmetic, with currencies and units: see [Types](/reference/types#arithmetic) |
| `== !=` | equality; quantities are compared after conversion (`1 kg == 1000 g`) |
| `< <= > >=` | order of numbers, amounts (same currency), quantities, prices, dates, durations, text, tuples (element by element) |
| `and`, `or` | short-circuit logic |
| `not a` | negation |
| `a implies b` | `not a or b`; `b` is evaluated only when `a` is true |
| `x in coll`, `x not in coll` | membership in a list or a set; `date in range` for a `DateRange` |
| `x is Variant`, `x is not Variant` | the value of an enum |
| `x is Entity`, `x is Trait` | the entity of a reference (also through traits) |
| `x is none`, `x is not none` | the value is missing (the same as `x == none`) |
| `a .. b` | an inclusive range; for whole numbers, the list `[a, …, b]` (`1..3` is `[1, 2, 3]`) |
| `x ?? y` | `x`, or `y` when `x` is `none` |
| `x?.name` | `none` when `x` is `none`, else `x.name` |

```quadra
reconcilable: Bool default type in {Receivable, Payable}
check currency == entry.company.currency implies amount_currency.raw == balance.raw,
    "In the company currency both amounts are equal"
check origin is not SalesLine or direction is Out, "A sales line on a vendor bill"
delivery_address: Address default customer.delivery_addresses.first() ?? customer.address
```

## Conditionals

### `a if cond else b`

```quadra
debit = balance if balance > 0 else 0
```

### `when:`

`when:` chooses the first arm whose condition holds. Each arm is `condition => value` on its
own line; `else => value` is the fallback. Without `else`, the value is `none` when no arm
holds.

```quadra
payment_state: enum(NotApplicable, Paid, Partial, NotPaid) = when:
    state is not Posted   => NotApplicable
    due == 0              => Paid
    paid > 0              => Partial
    else                  => NotPaid
```

In a `when:` arm, `ready => Done` reads `ready` as a condition (a field), not as a lambda.

### `match`

`match` compares a value with patterns, one arm per line. An arm's body is an expression on
the same line or an indented block.

```quadra
specificity: Int = match target:
    OneProduct(_) => 3
    InCategory(_) => 2
    AllProducts   => 1

direction: enum(In, Out) = match kind:
    CustomerInvoice | CustomerCredit => Out
    VendorBill | VendorCredit        => In
```

| Pattern | Matches |
|---|---|
| `Draft` | the variant (a name starting with an upper-case letter) |
| `Fixed(amount)` | a variant with data, binding its fields by position |
| `PurchaseLine(pl)` | a reference to that entity (or trait), bound to `pl` |
| `some(x)` | any value that is not `none`, bound to `x` |
| `none` | `none` |
| `"merce"`, `0`, `22%`, `true` | a literal |
| `_` | anything |
| `other` | anything, bound to `other` (a lower-case name) |
| `A \| B` | either pattern |

A `match` on an enum must cover every variant, or have `_` or a binding:
`non-exhaustive match: missing Cancelled`. On an optional enum, `none` must be covered too.
At run time, a value no arm matches fails with `no match arm for …`.

## Lambdas

`x => body` or `(a, b) => body`. Lambdas are passed to collection methods, used in
`where` modifiers and in permission rules, and can be stored in a `let`:

```quadra
lines.where(l => l.qty > 0 pcs).sum(l => l.subtotal)
let add = (a, b) => a + b
expect add(2, 3) == 5
```

## Collections, tuples and calls

| Form | Example |
|---|---|
| List | `[1, 2, 3]`, `[]` |
| Spread in a list | `[*a.map(…), *b.map(…), x]` |
| Set | `{Receivable, Payable}` |
| Tuple | `(1, "a")` |
| Named tuple | `(due: date, amount: total)`; fields read with `.due` |
| Index | `items[0]` (from 0; `none` past the end) |
| Positional and named arguments | `exchange_rate(USD, EUR, on=today())` |
| Record construction | `Customer(name="Anna")`: see [Entities](/reference/entities#creating-records) |

## Statements

Statements appear in actions, reactions, functions, tests, fixtures, seeds, migrations and
changes, and in multi-line formulas (only `let` there).

| Statement | Meaning |
|---|---|
| `let x = expr` · `let x: T = expr` | a local variable |
| `x = expr` · `x += expr` · `-=` | change a local variable |
| `record.field = expr`, `field = expr` | change a field (in an action, a bare field name is the record's) |
| `record.field += expr` · `-=` | add to or subtract from a field |
| `if cond:` … `else:` … | conditional; for more branches, nest an `if` inside the `else:` block |
| `if let v = optional:` … `else:` … | runs the block with `v` when the value is not `none` |
| `for x in coll:` | loop; `for a, b in pairs:` takes tuples apart |
| `return expr` | returns from a function or an action |
| `require cond, "message"` | fails with the message when `cond` is false; without a message: `requirement failed: <cond>` |
| `fail expr` | fails with the text of the value; `fail` is also an expression: `x ?? fail "missing"` |
| `emit Event(…)` | queues an event; see [Flows and actions](/reference/flows-and-actions#events) |
| `fn name(…):` | a local function |
| `as login:` … | runs the block with the permissions of a user ([Testing](/reference/testing#as-a-user)) |
| any expression | evaluated (a call of an action, a constructor) |

A failure (`fail`, `require`, a violated rule) undoes the whole transaction.

```quadra
fn sign(x: Int) -> Text:
    if x > 0:
        return "+"
    else if x < 0:
        return "-"
    else:
        return "0"
```

A line that starts with `else` followed by anything but `:`, `=>` or `if` continues the line
above: it is the `else` of an `a if c else b` written on two lines.

A `let` variable can be assigned again, also from inside a `for` or `if` block: the
assignment changes the variable declared outside it. `x += v` and `x -= v` add and subtract.

```quadra
fn total(xs: [Int]) -> Int:
    let t = 0
    for x in xs:
        t += x
    return t
```

## Collection methods

They apply to lists, sets, and to entity names (all records of the entity, or of every
entity implementing a trait).

| Method | Result | Notes |
|---|---|---|
| `.where(x => cond)` | the elements that satisfy `cond` | keeps a set a set |
| `.map(x => v)` | a list of the values | |
| `.flat_map(x => coll)` | a list: the collections concatenated | single values are kept, `none` is dropped |
| `.sum()` · `.sum(x => v)` | the sum of the values | `0` for an empty collection (for amounts, quantities and prices, the zero of their type: `0.00 EUR`, `0 pcs`); `none` values are skipped; currencies and units must agree ([Types](/reference/types#sums-over-records)) |
| `.count()` · `.count(x => cond)` | how many (that satisfy `cond`) | `Int` |
| `.any()` · `.any(x => cond)` | at least one element (that satisfies `cond`) | |
| `.all(x => cond)` | every element satisfies `cond` | true when empty |
| `.none()` · `.none(x => cond)` | no element (satisfies `cond`) | |
| `.find(x => cond)` | the first element that satisfies `cond`, or `none` | optional |
| `.single()` · `.single(x => cond)` | the only element (that satisfies `cond`) | fails with `single(): no element matches` or `single(): 2 elements match, expected one` |
| `.first()` · `.last()` | the first or last element, or `none` | optional |
| `.first(x => cond)` · `.last(x => cond)` | the first or last element that satisfies `cond`, or `none` | optional |
| `.first(n)` · `.last(n)` | a list of the first or last `n` elements | `n` a whole number; over an entity (after `where` and `sort_by`) one page of SQL |
| `.contains(x)` | `x` is an element | |
| `.distinct()` | the elements without duplicates, in order | a list |
| `.sort_by(x => key)` | the elements in ascending order of `key` | stable; a tuple key sorts by several keys; negate a number for descending order |
| `.last_by(x => key)` | the element with the greatest `key`, or `none` | |
| `.max(x => v)` · `.min(x => v)` · `.max()` · `.min()` | the greatest or smallest **value** `v`, or `none` | returns the value, not the element; `none` values are skipped |
| `.avg(x => v)` · `.avg()` | the mean of the values | `none` values are skipped; `none` when there is no value; whole numbers give a `Decimal`; amounts and quantities as for `sum` |
| `.group_by(x => key)` · `.group_by(x => key, x => key2)` | a list of groups `(key, items)` or `(key, key2, items)` | in order of first appearance; read with `g.key`, `g.key2`, `g.items`; [below](#group-by) |
| `.allocate(total, by=x => w)` | a list of `(item, share)` tuples | see [below](#allocate) |
| `.is_contiguous()` | whole numbers with no gaps and no duplicates | |
| `Entity.all()` | the list of all records of the entity | |

```quadra
tax_lines = lines.group_by(l => l.tax).map(g => (
    tax: g.key,
    base: g.items.sum(l => l.subtotal),
    amount: (g.items.sum(l => l.subtotal) * g.key.rate).round()))

best = pricelist.rules
    .where(r => r.matches(product, qty, on))
    .sort_by(r => (-r.specificity, -r.min_qty))
    .first()
```

### `group_by`

`coll.group_by(x => key)` splits a collection into groups of the elements with the same key,
in the order their first element appears; each group is a tuple `(key, items)`. With two keys,
`group_by(x => key, x => key2)`, each group is `(key, key2, items)`. A named tuple is a key
too: `group_by(x => (journal: x.journal, year: x.date.year))` and `g.key.journal`.

```quadra
let by_region = Order
    .where(o => o.state in {Confirmed, Shipped})
    .group_by(o => o.customer.region, o => o.date.month)
    .sort_by(g => -g.items.sum(o => o.total).raw)
    .first(10)
    .map(g => (region: g.key.name, month: g.key2, orders: g.items.count(), total: g.items.sum(o => o.total)))
```

Keys are values like any other: a record (`o.customer`), text, a choice, a number, yes or no,
a date — and the buckets of a date through the date's own fields and methods: `o.date.month`
(the year and month), `o.date.year`, `o.date.start_of_month()`.

**Over an entity, the database groups.** When the collection is an entity (`Order`, or
`Order.where(…)`), the keys are paths of fields (through references, as above) and the groups
are only read through their keys and aggregates of their rows — `g.items.count()`,
`g.items.sum(x => v)`, `g.items.avg(x => v)`, `g.items.min(x => v)`, `g.items.max(x => v)` — in
the stages that follow (`where`, `sort_by`, `first(n)`, `last(n)`, then `map` or `count`), the
whole chain is one `GROUP BY` of the database ([Storage](/reference/storage#loading-on-demand)),
with the same row permissions as the grid's groups. Otherwise (a key that is not a path, the
rows of a group read one by one: `g.items.map(…)`, a sum SQL cannot compute) the interpreter
groups the rows itself. The answer is the same either way.

In an expression a user writes (the [formulas of an app](/reference/apps-and-dashboards)), at
most 10 000 groups are made, and the interpreter reads at most 10 000 rows of an entity: beyond,
the formula fails and asks to filter first. In code (actions, formulas, controls) the
interpreter groups every row, as before.

### `allocate`

`coll.allocate(total, by=x => weight)` splits an amount over the elements, proportionally to
the weights, so that the shares add up to `total` exactly, to the smallest unit of the
currency. The remainders go, one cent at a time, to the elements with the largest
remainders, and among equal remainders to the first. Weights are numbers (use `.raw` for
amounts and quantities); without `by`, or when all weights are zero, the split is equal. The
result is a list of `(item, share)` tuples, in the order of the elements.

```quadra
let shares = items.allocate(10.00 EUR, by=i => i.qty.raw)    # weights 1, 2, 3
expect shares.map(s => s.share) == [1.67 EUR, 3.33 EUR, 5.00 EUR]
expect items.allocate(0.10 EUR).map(s => s.share) == [0.04 EUR, 0.03 EUR, 0.03 EUR]
```

The total must be an amount; a negative total gives negative shares.

## Methods of records

| Method | Result |
|---|---|
| `record.field`, `record.formula` | the value |
| `record.action(…)` | runs the action (see [Flows and actions](/reference/flows-and-actions#actions)) |
| `record.method(…)` | calls a `fn` declared in the entity |
| `record.copy(field=value, …)` | a new record with the same stored values ([Entities](/reference/entities#copy)) |
| `record.closure(x => next)` | every record reachable by applying `next` repeatedly, as a set |

`closure` follows a relation transitively. `next` may return a record, `none`, or a
collection. The starting record is included only if it can be reached again, which is how
cycles are detected:

```quadra
check not self.closure(c => c.parent).contains(self), "Cyclic category tree"
tree = {self} + self.closure(l => l.children)
```

## Methods of values

### Text

| Method | Result |
|---|---|
| `.len()` | the number of characters |
| `.upper()` · `.lower()` · `.trim()` | changed text |
| `.take(n)` | the first `n` characters |
| `.initials()` | the first letter of each word: `"Magazzino centrale".initials()` is `"Mc"` |
| `.starts_with(t)` · `.ends_with(t)` · `.contains(t)` | `Bool` |
| `.sequence_part()` | the number after the last `/`: `"FT/2026/00042".sequence_part()` is `42` |

### Numbers, amounts, quantities, prices

| Method or field | Applies to | Result |
|---|---|---|
| `.round()` | `Money`, `Price`, `Qty`, `Decimal`, `Percent` | rounded: see [Types](/reference/types#rounding-and-exact) (a `Percent` to 2 decimals of the percentage) |
| `.round(step=0.05)` | `Money` | rounded to a multiple of `step` |
| `.round_up()` | `Qty` | rounded upwards to the unit's precision |
| `.abs()` | `Money`, `Price`, `Qty`, numbers | the absolute value |
| `.to(unit)` | `Qty`, `Price`, `Duration` | converted ([Types](/reference/types#conversions)) |
| `.as_percent()` | `Decimal`, `Int`, `Percent` | the same number as a `Percent`: `0.25.as_percent()` is `25%`, `3.as_percent()` is `300%` |
| `.raw` | `Money`, `Price`, `Qty`, numbers | the bare number |
| `.currency` | `Money` | the `Currency` record |

### Dates

| Method or field | Applies to | Result |
|---|---|---|
| `.year` | `Date`, `DateTime` | `Int` |
| `.month` | `Date`, `DateTime` | the year and month: `2026-11-21.month == 2026-11` |
| `.date` | `DateTime` | the `Date` |
| `.start_of_month()` | `Date`, `DateTime` | the first day of the month (a `DateTime`: at 00:00:00) |
| `.end_of_month()` | `Date`, `DateTime` | the last day of the month (a `DateTime`: at 23:59:59) |
| `.contains(d)` | `DateRange` | `d` is in the range (also `d in range`) |

### Other values

| Field | Applies to |
|---|---|
| `.key`, `.key2`, `.items` | the groups of `group_by` |
| `.item`, `.share` | the results of `allocate` |
| `.name` | the named fields of a tuple |
| `.is_provisional` | a field filled with `provisional(…)` in a migration: `p.hs_code.is_provisional` |

## Built-in functions

| Function | Result | Notes |
|---|---|---|
| `today()` | the current `Date` | not allowed in a formula |
| `now()` | the current `DateTime` | not allowed in a formula |
| `convert(x, to=C, on=d)` · `convert(x, to=C, rate=r)` | `x` (`Money` or `Price`) in currency `C`, `Exact` | in a formula, `on=` or `rate=` is required |
| `exchange_rate(src, dst, on)` | `Decimal` | from the standard library ([Types](/reference/types#conversions)) |
| `next_number(company, code, on=today())` | the next number of a sequence, as `Code` | see below |
| `Qty(value, unit)` | a quantity | when the unit is known only at run time: `Qty(1, product.uom)` |
| `Price(amount, per=unit)` | a price | |
| `DateRange(from, to)` | a range of dates, both included | |
| `why(record)` | the record's cause chain, as text | who created or changed it, and why |
| `login(login, company=c)` | — | in tests and fixtures: the user and company of the following statements |
| `provisional(value, reason=…, until=…)` | `value`, marked as provisional | in migrations ([Migrations](/reference/migrations-and-changes#provisional-values)) |
| `affected(Entity)` | how many records of the entity the change created or changed | in the `expect:` block of a change |
| `add(column=value, …)` | — | only in a `post to` block ([Ledgers](/reference/ledgers)) |
| a metric | its value | `revenue(company, period)` ([Metrics](/reference/metrics)) |

`today()` and `now()` are forbidden in formulas, because a formula must always give the same
result from the same data (which is also what lets the engine keep it stored and up to date,
[Formulas kept up to date](/reference/storage#formulas-kept-up-to-date)):

```text
error: now() is not allowed in a formula: formulas must give the same result every time (use `default` or an action)
```

They are allowed in defaults, actions, reactions and tests. In tests the clock is set with
[`clock`](/reference/testing#clock).

### `next_number`

```quadra
action post:
    number = next_number(company, journal.code)
```

`next_number(company, code)` looks up the `Sequence` with that `company` and `code` and fills
its `pattern`:

| Placeholder | Replaced by |
|---|---|
| `{code}` | the code |
| `{year}` | the year of `on` |
| `{n}` | the counter |
| `{n:05}` | the counter on 5 digits (`00042`); any [f-string format](/reference/syntax#f-strings) works (`{n:03}`, `{n:6}`) |

The counter restarts every year, for each company and code. Without a `Sequence`, the
pattern is `{code}/{year}/{n:05}`. The counter is part of the transaction: if the
transaction fails, the number is not used, so failed attempts leave no gaps (every sequence
is gapless, whatever its `gapless` field says).

### The session: `current` and `me`

| Expression | Value |
|---|---|
| `current.user`, `me` | the `User` running the request, the action or the test statement; `none` for the system |
| `current.company` | the company of the session |

They are used in defaults (`company: Company default current.company`), in actions, and in
permission rules (`where o => o.salesperson == me`).
