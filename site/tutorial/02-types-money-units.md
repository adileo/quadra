# Step 2: Money, quantities and units

<SandboxLink project="tutorial-02" file="shop.qd" />

**Goal:** give products a price. Our shop sells mugs by the piece and coffee by the kilogram,
and a customer may buy 250 grams. We want the compiler to make sure we never mix euros with
dollars, kilograms with pieces, or forget to round a cent.

## Types that know what they measure

Quadra has types for the values business software deals with all day:

| Type | Example literals | What it is |
|---|---|---|
| `Money<EUR>` | `10.50 EUR`, `1_000.00 EUR` | an amount in a currency |
| `Qty<kg>` | `2.5 kg`, `250 g`, `3 pcs` | a quantity in a unit |
| `Price<EUR, kg>` | `18.00 EUR/kg`, `7.50 EUR/pcs` | money per unit (no spaces around `/`) |
| `Percent` | `22%`, `0.5%` | a percentage |
| `Date`, `DateTime` | `2026-11-21`, `2026-11-21 09:00` | a day, a moment |
| `Duration` | `30 days`, `8 hours` | a length of time (shown as `30 days`, `8 h`) |
| `Int`, `Decimal`, `Bool`, `Text` | `42`, `0.92`, `true`, `"hi"` | the usual ones |

Currencies (`EUR`, `USD`, `GBP`, `CHF`, `JPY`, `CNY`) and units (`pcs`, `kg`, `g`, `t`, `l`, `ml`,
`m`, `cm`, `h`, `min`) come from the standard library. Arithmetic follows the units:
`Qty<kg> * Price<EUR, kg>` is `Money<EUR>`, and grams convert to kilograms on their own.

Numbers are **exact decimals**: `0.10 EUR + 0.20 EUR` is exactly `0.30 EUR`.

## The code

`shop.qd` gets new fields (highlighted):

```quadra{7-9,17-25}
# Our shop: the people we sell to and the things we sell.

entity Customer:
    name: Text unique
    email: Email?
    city: Text?
    since: Date default today()
    discount: Percent default 0%
    credit_limit: Money<EUR> default 500.00 EUR


entity Product:
    code: Code unique
    name: Text
    description: Text?
    active: Bool default true
    uom: Uom default pcs                    # the unit we sell it in: pcs, kg, l…
    price: Price<EUR, uom>                  # EUR per unit: 7.50 EUR/pcs, 18.00 EUR/kg
    cost: Price<EUR, uom> default 0         # what we pay for it
    weight: Qty<kg>?                        # always in kilograms, whatever we sell it in
    vat: Percent default 22%

    # What `qty` of this product costs, rounded to the cent.
    fn price_for(qty: Qty<uom>) -> Money<EUR>:
        return (qty * price).round()
```

The interesting line is `price: Price<EUR, uom>`. The unit of the price is **another field of
the same record**: a mug's price is in `EUR/pcs`, a coffee's in `EUR/kg`. The compiler follows
these *dependent units*: in `price_for`, `qty` is a `Qty<uom>` and `price` a `Price<EUR, uom>`,
so it can prove that the units match.

`uom: Uom default pcs` is a reference to a unit of measure record (`Uom` is an entity of the
standard library, `pcs` one of its records).

`fn` declares a **function**: a method of `Product` here. Functions are pure: they compute,
they don't change data.

## Rounding is explicit

`qty * price` is not quite money yet: `0.333 kg * 18.00 EUR/kg` is `5.994 EUR`. Quadra calls it
`Exact<Money<EUR>>` and refuses to store or return it until you say how to round. Remove the
`.round()` from `price_for` and run `quadra check`:

```text
error: Exact<Money<EUR>> must be rounded with .round() before it is stored as Money<EUR>
   ┌─ shop.qd:25:16
   │
25 │         return qty * price
   │                ^^^^^^^^^^^

✗ 1 errors, 0 warnings in 6 modules
```

`.round()` rounds to the currency's decimals (2 for euros, 0 for yen), half to even unless
the currency's `rounding` says otherwise.

## What the compiler refuses

Try adding euros to dollars anywhere in the project:

```quadra
let total = 7.50 EUR + 2.00 USD
```

```text
error: cannot add Money<EUR> and Money<USD>
   ┌─ tests/types.test.qd:34:17
   │
34 │     let total = 7.50 EUR + 2.00 USD
   │                 ^^^^^^^^^^^^^^^^^^^
```

Other mistakes it catches, with the messages you get:

| Code | Error |
|---|---|
| `2 kg + 3 pcs` | `cannot add Qty<kg> and Qty<pcs>` |
| `2 kg * 7.50 EUR/pcs` | `cannot multiply Qty<kg> by Price<EUR, pcs>` |
| `Customer(name="Bob", credit_limit=500)` | `cannot store Int in a field of type Money<EUR>` |
| `return q * p.price` in a function returning `Money<EUR>` | `… must be rounded with .round() …` |

## Tests

You can turn those compiler errors into tests with `expect_compile_error`: the test passes
only if the code inside does **not** compile, with that message. Create
`tests/types.test.qd`:

```quadra
test "prices, quantities and units":
    let beans = Product(code="BEANS-01", name="House blend", uom=kg, price=18.00 EUR/kg, vat=10%)
    let mug = Product(code="MUG-01", name="Espresso mug", price=7.50 EUR/pcs)
    expect mug.uom == pcs                           # the default
    expect mug.price_for(3 pcs) == 22.50 EUR
    expect beans.price_for(250 g) == 4.50 EUR       # grams are converted to kilograms
    let parcel = 1500 g
    expect parcel.to(kg) == 1.5 kg

test "money is exact":
    expect 10.00 EUR * 22% == 2.20 EUR
    let third = 10.00 EUR / 3
    expect third.round() == 3.33 EUR
    expect 0.10 EUR + 0.20 EUR == 0.30 EUR          # no floating point surprises

test "dates and durations":
    expect 2026-11-21 + 30 days == 2026-12-21
    expect 2026-12-21 - 2026-11-21 == 30 days

test "the compiler catches unit and currency mistakes":
    expect_compile_error "cannot add Money<EUR> and Money<USD>":
        let x = 10.00 EUR + 5.00 USD
    expect_compile_error "cannot add Qty<kg> and Qty<pcs>":
        let y = 2 kg + 3 pcs
    expect_compile_error "cannot multiply Qty<kg> by Price<EUR, pcs>":
        let z = 2 kg * 7.50 EUR/pcs
    expect_compile_error "must be rounded with .round()":
        fn total(p: Product, q: Qty<p.uom>) -> Money<EUR>:
            return q * p.price
    expect_compile_error "cannot store Int in a field of type Money<EUR>":
        let c = Customer(name="Bob", credit_limit=500)
```

`Product` now needs a price, so the product in `tests/shop.test.qd` gets one:
`Product(code="MUG-01", name="Espresso mug", price=7.50 EUR/pcs)`.

```bash
quadra test
```

```text
✓ a new customer
✓ new products are active
✓ prices, quantities and units
✓ money is exact
✓ dates and durations
✓ the compiler catches unit and currency mistakes

6 passed, 0 failed
```

::: tip Parentheses after `expect`
`expect (1500 g).to(kg) == 1.5 kg` works: a statement keyword followed by `(` still starts
the statement.
:::

## Try it

1. Add a product sold by the litre (`uom=l`) and test `price_for(500 ml)`.
2. Give the mug a `weight=200 g` and test that `mug.weight == 0.2 kg`. Then try
   `weight=1 pcs`: what does `quadra check` say?
3. Write an `expect_compile_error` for `1.00 EUR * 2 kg + 3.00 USD`.

## Recap

- `Money<C>`, `Qty<U>` and `Price<C, U>` carry their currency and unit; the compiler checks
  every operation.
- Units can depend on a field (`Price<EUR, uom>`), and conversions within a kind of unit
  (`g` → `kg`) are automatic.
- Money is exact; computed amounts are `Exact<…>` until you `.round()` them.
- `expect_compile_error` turns "this must not compile" into a test.

[Next: relations →](./03-relations)
