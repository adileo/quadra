# Syntax

Quadra source files are UTF-8 text with the `.qd` extension. Blocks are delimited by
indentation, as in Python. This page covers the lexical structure: lines, indentation,
comments, keywords and literals. Declarations are described in [Entities](/reference/entities),
expressions in [Expressions](/reference/expressions).

## Lines and indentation

A block starts with `:` at the end of a line and contains the following lines that are
indented more deeply:

```quadra
entity Order:
    customer: Customer
    state: enum(Draft, Confirmed) default Draft

    action confirm:
        require lines.count() > 0, "Order has no lines"
```

| Rule | Detail |
|---|---|
| Spaces only | A tab in the indentation of a line is an error: `tabs are not allowed in indentation, use spaces`. |
| Width | Any width works; 4 spaces per level is the convention. |
| Dedent | A line must return to an indentation level used before: `this line's indentation does not match any outer block`. |
| Short blocks | A block can be a single statement on the same line: `as anna: inv.post()`. |
| Blank lines and comments | Ignored by the layout: they never open or close a block. |

### Line continuation

A line break does not end a statement when:

1. it is inside `( )`, `[ ]` or `{ }`;
2. the line ends with `,` or with a binary operator: `+ - / ?? == != <= >= and or implies`
   (not `<`, `>` or `*`, which can legitimately end a line, as in `Money<EUR>` or `read *`);
3. the next line starts with `.` or `?.` (method chains);
4. the next line starts with a binary operator: `+ - * / ?? == != < <= > >= and or implies`;
5. the next line starts with `else` that is not followed by `:` or `=>` (the `a if c else b`
   form).

```quadra
total = lines
    .where(l => l.qty > 0 pcs)
    .sum(l => l.amount)

incoming =
    StockMove.where(m => m.product == product and m.state is Ready).sum(m => m.qty_base)
  + PurchaseLine.where(l => l.product == product).sum(l => l.qty.to(product.uom))

partner_account: Account =
    (partner.as_customer?.account ?? company.receivable) if direction is Out
    else (partner.as_vendor?.account ?? company.payable)
```

### Values on several lines

A line that ends with `=` (a formula) or `default` (an initial value) is **not** continued:
the indented lines that follow form a block, and the value is the block's last expression.
The block may contain `let` statements.

```quadra
tax_share =
    let g = invoice.tax_lines.single(t => t.tax == tax)
    g.items.allocate(g.amount, by=l => l.subtotal.raw).single(a => a.item == self).share

expected: Qty<product.uom> default
    Stock.where(s => s.product == product and s.location == count.location).sum(s => s.qty)
```

## Comments

`#` starts a comment that runs to the end of the line. Comments are kept by the parser (the
formatter and the editor see them). Comment lines written directly above a declaration, with
no blank line in between, are its documentation: the editor shows them on hover.

```quadra
# Payment terms: the instalments always add up to the amount.
entity PaymentTerm:
    name: Text unique          # "30/60 end of month"
```

## Identifiers

An identifier starts with a letter or `_` and continues with letters, digits and `_`. Letters
include non-ASCII letters. See [Naming](/reference/projects-and-modules#naming) for the
conventions.

## Keywords

These words are reserved everywhere and cannot be used as names:

```text
and as const else entity enum extend false fn for from if import in is
ledger let match none not or record return self trait true when where
```

Every other keyword is **contextual**: it has its special meaning only where it is expected,
and is an ordinary name elsewhere. This keeps common words available as field names.

| Contextual keywords | Where |
|---|---|
| `temporary`, `builtin`, `seed`, `event`, `outbound`, `inbound`, `on`, `before`, `role`, `deny`, `allow`, `view`, `report`, `metric`, `control`, `migration`, `change`, `fixture`, `test`, `patch` | at the start of a top-level declaration |
| `check`, `warn`, `unique`, `frozen`, `live`, `flow`, `action`, `post`, `load`, `key`, `measure`, `attr` | at the start of a member |
| `default`, `required`, `parent`, `per`, `company`, `via`, `through`, `except` | in a field or a `frozen` line |
| `by`, `unless` | in a flow transition |
| `enters`, `becomes`, `created`, `changes`, `converges`, `delivered`, `failed` | in a reaction |
| `require`, `fail`, `emit`, `use`, `clock`, `expect`, `expect_error`, `expect_compile_error` | at the start of a statement |
| `rename`, `convert`, `backfill`, `verify` | in a migration |
| `read`, `create`, `edit`, `run`, `all`, `hide`, `show`, `history` | in a permission rule |
| `extends` | in a fixture |
| `via`, `path`, `reply`, `retry`, `times`, `backoff`, `expires`, `after`, `unless`, `delay`, `window`, `key`, `ordered`, `latest`, `by` | in an `outbound` (`via`, `path`, `key` in an `inbound` too) |
| `at` | in a field of a record `from json` (`from` after a record's name is the reserved word) |
| `implies` | as a binary operator |

So `key` and `items` (the parts of a group), `to` (in `convert(x, to=EUR)`), or `parent`
(a field name) all work as ordinary names.

A statement keyword at the start of a statement always starts that statement, also when a
`(` or `[` follows: `expect (a + b) == c` checks `(a + b) == c`, and
`expect [1, 2].count() == 2` checks the count. It is an ordinary name only when it is used as
one: followed by `.`, `=`, `:=`, `+=`, `-=`, `,`, `)` or the end of the line.

## Literals

| Literal | Examples | Type |
|---|---|---|
| Integer | `42` `1_000` | `Int` |
| Decimal | `0.92` `1_000.50` | `Decimal` |
| Percent | `22%` `0.5%` | `Percent` (`22%` is the number 0.22) |
| Money | `10.50 EUR` `50_000 USD` | `Money<EUR>` |
| Quantity | `3 pcs` `2.5 kg` `10 min` | `Qty<pcs>` |
| Price (per unit) | `9.90 EUR/pcs` `60.00 EUR/h` | `Price<EUR, pcs>` |
| Duration | `8 hours` `30 minutes` `1 day` `2 weeks` | `Duration` |
| Text | `"Milano"` `"""…"""` | `Text` |
| F-string | `f"Credit limit {limit} exceeded"` | `Text` |
| Date | `2026-11-21` | `Date` |
| Date and time | `2026-11-21 09:00` `2026-11-21T09:00:30` | `DateTime` |
| Year and month | `2026-11` | year-month (the type of `date.month`) |
| Boolean | `true` `false` | `Bool` |
| No value | `none` | the empty value of an optional type |

### Numbers

Digits with an optional fractional part. `_` may separate digits anywhere: `1_000_000.00`.
There is no exponent notation and no hexadecimal. Numbers are exact decimals, never
floating point. A number immediately followed by `%` (no space) is a percent; there is no
modulo operator.

A negative number is the unary minus applied to a literal: `-3 pcs`.

### Amounts, quantities and prices

A number followed by a space and a unit or currency code, on the same line, is a measured
value. Whether the code is a currency or a unit of measure depends on what is seeded (see
[Types](/reference/types#where-currencies-and-units-come-from)):

```quadra
10.50 EUR          # Money<EUR>
3 pcs              # Qty<pcs>
2.5 kg             # Qty<kg>
```

Two codes joined by `/` **with no spaces** form a price per unit. With spaces, `/` is a
division:

```quadra
9.90 EUR/pcs       # Price<EUR, pcs>: 9.90 EUR per piece
60.00 EUR/h        # Price<EUR, h>
10.00 EUR / qty    # Money<EUR> divided by the value of `qty`
```

`second(s)`, `minute(s)`, `hour(s)`, `day(s)` and `week(s)` make a `Duration` instead of a quantity:
`8 hours`, `1 hour + 30 minutes`, `30 days`, `1 week`. (`h` and `min` are units of measure:
`10 min` is a `Qty<min>`; a duration becomes one with `.to(h)`.)

A number followed by one of these words does **not** take it as a unit, so that field
modifiers can follow numbers: `implies`, `default`, `required`, `unless`, `by`, `per`,
`except`, `live`, `via`, `through`, `parent`, `unique`, `converges`. For example
`level: Int where 0..4 default 2`.

An unknown code is an error: `` unknown unit or currency `XYZ` ``.

### Text

Strings are written between double quotes. The escapes are `\n` (new line), `\t` (tab),
`\r`, `\"` and `\\`; a backslash before any other character is kept as written. A string may
not span lines, unless it is triple-quoted.

Triple-quoted strings (`"""…"""`) span several lines. The common indentation of their
lines is removed, and blank first and last lines are dropped:

```quadra
definition: """
    Sum of the net amounts of the posted customer invoices dated in the period,
    minus credit notes, converted at the rate of each invoice.
    """
```

### F-strings

`f"…"` interpolates expressions written between `{` and `}`. Values are shown the way the
application displays them: an amount as `10.50 EUR`, a record by its `number`, `code`,
`name` or `login` (the first one it has).

```quadra
require exposure <= limit, f"Credit limit exceeded: exposure {exposure}, limit {limit}"
fail f"Missing exchange rate {src.code}/{dst.code} on {on}"
```

A format can follow the expression after a `:`: `{expr:spec}`, where `spec` is an optional
alignment (`<`, `>` or `^`), an optional `0`, an optional width and an optional `.precision`.

| Format | Meaning | Example |
|---|---|---|
| `.p` | `p` decimals (rounded half-even) of a number, percentage, amount, price or quantity | `f"{3.14159:.2}"` → `3.14`, `f"{12.5 EUR:.0}"` → `12 EUR` |
| `width` | at least `width` characters: numbers and measures right-aligned, the rest left-aligned | `f"[{42:5}]"` → `[   42]`, `f"[{code:6}]"` → `[AB    ]` |
| `0width` | a number padded with zeros after the sign | `f"{42:05}"` → `00042`, `f"{-7:04}"` → `-007` |
| `<`, `>`, `^` | left, right or centred alignment in the width | `f"[{42:<5}]"` → `[42   ]`, `f"[{42:^6}]"` → `[  42  ]` |

`.p` on a value that is not a number, an amount, a price or a quantity, or `0` padding on
anything but a plain number, is a compile error, and so is a format that is not of this shape
(`` invalid format `xyz` in f-string ``) or an expression that does not parse
(`` invalid expression in f-string ``).

::: v-pre
`{{` and `}}` write a literal brace (`f"{{n}} = {n}"` → `{n} = 42`), and the escapes of
strings (`\n`, `\t`, `\"`, `\\`) work in f-strings too.
:::

The patterns of [sequences](/reference/projects-and-modules#the-standard-library) use the same
formats: `"FT/{year}/{n:05}"` numbers `FT/2026/00042`.

### Dates and times

| Form | Example | Meaning |
|---|---|---|
| `YYYY-MM-DD` | `2026-11-21` | a date |
| `YYYY-MM-DD hh:mm` | `2026-11-21 09:00` | a date and time (one space) |
| `YYYY-MM-DDThh:mm[:ss]` | `2026-11-21T09:00:30` | a date and time |
| `YYYY-MM` | `2026-11` | a year and month |

Because `2026-11` is a year-month literal, write a subtraction of two numbers with spaces:
`2026 - 11`.

Date and time values have no time zone.
