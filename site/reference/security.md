# Security

Permissions are declared in the model, with the same expressions as formulas, so they apply
everywhere a user acts: the web client, the API, actions they run. Without a rule that allows
it, a user can do nothing.

::: tip Before the first role
A model that declares no `role` at all does not model permissions yet: every user may do
everything (top-level `deny` rules still apply). This is how a new project starts
(`quadra init`). Declaring the first role turns the rules on.
:::

```quadra
role Employee:
    read Product, Category, Partner, Warehouse

role Salesperson is Employee:
    read, create, edit SalesOrder where o => o.salesperson == me
    run SalesOrder.send, SalesOrder.confirm where o => o.salesperson == me
    read Invoice where i => i.direction is Out and i.partner.sales_orders.any(o => o.salesperson == me)
    hide Product.standard_cost, Product.cost

role SalesManager is Salesperson:
    read, edit SalesOrder
    run SalesOrder.lock, SalesOrder.cancel, SalesOrder.create_invoice
    show Product.cost
    deny run SalesOrder.cancel where o => o.state is Locked

role Auditor:
    read *
    read history *

# separation of duties: rules for everybody, whatever their roles
deny run PurchaseOrder.approve where o => o.created_by == me
deny edit ExchangeRate where r => r.date < today() - 1 day

# for everybody, whatever their roles
allow read Currency, Uom
```

## Users and roles

Users are records of the standard library's `User` entity; their `roles` field is a set of
roles:

```quadra
User(login="giulia", roles={Salesperson, Buyer})
User(login="marco",  roles={PurchaseApprover, SalesManager})
```

`role Name:` declares a role and its rules; `role Name is Parent:` also grants every rule of
`Parent` (and of its parents). A user has the rules of all their roles.

| Name | Value |
|---|---|
| `me`, `current.user` | the user making the request |
| `current.company` | the session's company |

## Rules

```text
[deny | allow] verb[, verb…] [history] target[, target…] [where x => condition]
hide | show target[, target…]
```

| Verb | Allows |
|---|---|
| `read` | reading records |
| `create` | creating records |
| `edit` | changing fields (`edit Entity` covers every field) |
| `run` | running an action (`run Entity.action`) |
| `erase` | erasing the personal data of a record (`erase Customer`: [Retention and erasure](/reference/retention#erase)) |
| `all` | every verb |
| `hide` | hides fields from the role's users: `hide Product.cost` |
| `show` | gives back fields that another role of the user hides: `show Product.cost` |
| `read history` | the history of records (every version and cause) |

`delete` is the permission of the entities whose records can be deleted (`deletable`:
[Retention and erasure](/reference/retention#deleting-records-deletable)): `delete Page`
lets a role delete a page, send it to the trash and restore it. On an entity that is not
`deletable` it is an error (`records of Order are not deleted, so there is no `delete`
permission on it: …`): its records are cancelled, reversed or deactivated by an action;
permit the action instead (`run Order.cancel`).
`derived` is not a permission either. `hide` and `show` stand alone on their line, inside a
role.

| Target | Meaning |
|---|---|
| `*` | every entity |
| `SalesOrder` | the entity |
| `SalesOrder.confirm` | an action (for `run`) |
| `Product.cost` | a field (for `edit` and `hide`) |
| `Partner.as_customer.credit_limit` | a part of a record field (matched by the field, `as_customer`) |

The optional `where` is a lambda that receives the record and must be true for the rule to
apply: `where o => o.salesperson == me`. It can follow references, filter collections and use
`me`, `current` and `today()`.

| Form | Where | Effect |
|---|---|---|
| a rule inside `role` (optionally written `allow …`) | the role's users | allows |
| `deny …` inside `role` | the role's users (and the users of the roles that inherit it) | refuses, before any rule that allows |
| `allow …` at the top level | every user, whatever their roles | allows |
| `deny …` at the top level | every user, whatever their roles | refuses, before any rule that allows |

A `deny` always wins over an `allow`: a user with two roles, one that allows editing a
field and one that denies it, cannot edit it. `deny read history X` refuses only the
history of `X`, not reading its records.

## How a request is decided

For a verb on a record (`edit SalesOrder.notes` of order 71, `run SalesOrder.confirm` of
order 71):

1. if a `deny` rule matches (at the top level, or in one of the user's roles) and its
   condition holds, the request is refused:
   `permission denied: deny run PurchaseOrder.approve (giulia)`;
2. if a rule of one of the user's roles, or a top-level `allow`, matches (same verb or
   `all`, same target or `*`) and its condition holds, the request is allowed. A rule
   `edit Entity` also allows editing any field of it;
3. if the record is a line of a document (it has a `parent` field), the document decides:
   reading a line needs `read` on the document, creating or changing a line needs `edit` on
   the document;
4. otherwise the request is refused: `permission denied: bob cannot edit Order.note`.

### Lines follow their document

Lines need no rules of their own: whoever may edit an order may add, change and read its
lines.

```quadra
role Sales:
    create, edit Order where o => o.salesperson == me

# as a Sales user, on one of their orders:
o.note = "urgent"           # edit Order
Line(order=o, qty=2)        # creating a line = editing the order
```

### When permissions are checked

| Operation | Checked |
|---|---|
| Creating a record | `create`, after the record is built (defaults included), so the condition sees it |
| Assigning a field | `edit` on the field |
| Running an action | `run` on the action, once, before it starts |
| Reading through the API | `read` on every record returned; `hide` on every field |
| Reading the history | `read history`, or `all` |

Code that runs as part of an action, a reaction, a fixture, a seed, a migration or a function
is not checked again: the action is the unit of permission. A user who may run
`SalesOrder.confirm` confirms the order even if the action changes records they could not
edit themselves.

Code with no user (seeds, migrations, `quadra` commands, tests outside `as`) is not checked.

## Hidden fields

`hide Entity.field, …` removes fields for the users of the role: they are left out of
`GET /v1/model`, of records, of their history and of the default columns of a query. A query
that asks for a hidden field, or for a path through a record the user may not read
(`customer.credit_limit`), gets the cell `{"hidden": true}`. Hiding applies to traits too:
`hide Document.notes` hides the field in every entity that implements `Document`.

`show Entity.field, …` gives a field back: a field is hidden from a user when one of their
roles hides it **and none of their roles shows it**. Roles are inherited, so `show` is how a
role undoes the `hide` of the role it extends:

```quadra
role Salesperson is Employee:
    hide Product.standard_cost, Product.cost

role SalesManager is Salesperson:
    show Product.cost            # sees the cost, still not the standard cost
```

A user who is both `Salesperson` and `Buyer` (with `show Product.cost`) sees the cost too.

## Testing permissions

Tests run without a user, so nothing is refused, until a block runs `as` a user:

```quadra
test "only the salesperson edits the order":
    User(login="ann", roles={Sales})
    User(login="bob", roles={Sales})
    let o = as ann: Order(lines=[Line(qty=1)])
    as ann:
        o.note = "ok"
    expect_error "permission denied: bob cannot edit Order.note":
        as bob: o.note = "no"
```

See [Testing](/reference/testing#as-a-user).

## In the API

Every API request runs as the authenticated user, with the same rules: a query returns only
the rows the user may read, `GET /v1/model` lists only the entities they may read with what
they may do (`can_create`, `editable`, actions), and a refused write answers `403` with the
message. Tokens can be read-only. See [HTTP API](/reference/http-api).

A `deny … where` depends on the record: it refuses the records its condition holds for, not
the entity as a whole. A role with `read *` and `deny read Order where o => o.customer.region.locked`
still lists `Order` in `GET /v1/model` and queries it; the orders of locked regions are left out.

## On large data

When a query, a search or a picker lists rows, the user's rules for `read` are applied by the
database, not record by record: on a database, the rules of all the user's roles (inherited
ones included) become one SQL condition — `(allow₁ or allow₂ or …) and not (deny₁ or …)`, a
line's rows those whose document is readable (through the `parent` chain) — with `me`,
`today()` and every other value that does not depend on the row bound as parameters. Totals
are then exact up to 10 000 rows and aggregates are computed in SQL under the permissions.
Opening one record still checks it with the rules directly; both always agree.

A condition runs in the database when it is made of what queries translate: stored fields
compared with values (`c.salesperson == me`, `o.state in {Draft, Sent}`), paths through
references of any length (`s.line.order.customer.region.logistics == me`), sets and
collections (`c.region.team.contains(me)`, `i.partner.sales_orders.any(o => o.salesperson == me)`),
`and`, `or`, `not`, `is none`, `created_by`. A rule that reads a formula (`i.direction is Out`
where `direction` is computed), a per-company field, a function of yours or a value stored as
a whole record is evaluated on each candidate row instead: correct, slower on large entities.

Every reference column is indexed, and so are the fields a rule compares with `==`
(`salesperson`, `manager`) and the members of sets: the database starts from the user's own
rows. On SQLite each rule is a set of ids (`id IN (SELECT …)`), on PostgreSQL a join; SQLite
keeps statistics for its planner (`ANALYZE` after an import, `PRAGMA optimize` at start).
