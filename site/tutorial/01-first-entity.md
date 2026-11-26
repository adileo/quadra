# Step 1: Your first entity

<SandboxLink project="tutorial-01" file="shop.qd" />

**Goal:** describe the two things every shop has — customers and products — and check that
the description is right.

## The project

A Quadra project is a folder with a `quadra.toml`:

```toml
[project]
name = "shop"
```

## Entities and fields

An **entity** is a kind of thing your business keeps track of. Each one becomes a table in the
database, a resource in the API and a sheet in the web client. Create `shop.qd`:

```quadra
# Our shop: the people we sell to and the things we sell.

entity Customer:
    name: Text unique
    email: Email?
    city: Text?


entity Product:
    code: Code unique
    name: Text
    description: Text?
    active: Bool default true
```

Reading it line by line:

- `name: Text` — a field and its type. Fields are **required** unless you say otherwise.
- `Text?` — the `?` makes the field **optional**: it may be `none`.
- `unique` — no two customers can have the same name.
- `Email`, `Code` — text types with a meaning: an email address, a short identifier.
- `default true` — the value a new product gets when you don't give one.

Blocks are made by indentation, as in Python: four spaces, never tabs.

::: tip What you don't write
Every entity also has an `id`, `created_at`, `created_by`, `updated_at`, `updated_by` and a
`version`, and Quadra keeps the full history of every field, with the cause of each change.
You never declare them.
:::

## Check it

```bash
quadra check
```

```text
✓ 4 modules checked, 0 warnings
```

`quadra check` reads every module of the project — yours plus the standard library, which gives
you currencies, units of measure, companies and users — and checks names, types and much more.
It's what your editor runs while you type.

## A first test

The fastest way to see a model work is a test. Tests live in `tests/`; create
`tests/shop.test.qd`:

```quadra
test "a new customer":
    let anna = Customer(name="Anna Rossi", email="anna@example.com")
    expect anna.name == "Anna Rossi"
    expect anna.city == none

test "new products are active":
    let mug = Product(code="MUG-01", name="Espresso mug")
    expect mug.active
    expect Product.count() == 1
```

- `Customer(name=…, email=…)` creates a record. Fields you leave out get their default, or
  `none` if they are optional.
- `let` gives a name to a value inside the test.
- `expect` fails the test if the condition is false.
- `Product.count()` counts the records of an entity.

```bash
quadra test
```

```text
✓ a new customer
✓ new products are active

2 passed, 0 failed
```

Every test starts from an empty world, and every statement in a test is one transaction, like
a click on Save.

## When something is wrong

Misspell a field in the test — `expect anna.nmae == "Anna Rossi"` — and run `quadra check`:

```text
error: `Customer` has no field `nmae`
  ┌─ tests/shop.test.qd:3:12
  │
3 │     expect anna.nmae == "Anna Rossi"
  │            ^^^^^^^^^

✗ 1 errors, 0 warnings in 5 modules
```

And when an expectation is false, `quadra test` shows both sides. With
`expect mug.active == false`:

```text
✓ a new customer
✗ new products are active  (tests/shop.test.qd:6)
    tests/shop.test.qd:8: expectation failed: mug.active == false
          left:  true
          right: false

1 passed, 1 failed
```

Rules declared in the model are enforced too. Creating two customers with the same name, or
one without a name, fails with:

```text
Customer.name must be unique: Anna already exists
Customer.name is required
```

## Try it

1. Add a `phone: Phone?` field to `Customer` and set it in the first test.
2. Write a test that creates two products and expects `Product.count() == 2`.
3. Remove `unique` from `Customer.name`: which test would you write to show the difference?

## Recap

- A project is a folder with `quadra.toml` and `.qd` modules.
- `entity` declares a kind of record; fields are required unless they end with `?`.
- `unique` and `default` are part of the field declaration.
- `quadra check` checks the model; `quadra test` runs the `test` blocks in `tests/`.

[Next: money, quantities and units →](./02-types-money-units)
