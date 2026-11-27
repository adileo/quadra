# Flows and actions

Business documents move through states: a quotation is sent, confirmed, delivered,
invoiced. A **flow** declares which moves are possible and what triggers them; **actions**
are the operations users and programs run on a record; **reactions** run automatically when
something happens; **events** carry messages to the outside world.

```quadra
entity SalesOrder is Document:
    customer: Partner
    lines: [SalesLine]
    state: enum(Draft, Sent, Confirmed, Locked, Cancelled) default Draft
    confirmed_at: DateTime? required when state in {Confirmed, Locked}

    flow state:
        Draft -> Sent                           by send
        Draft | Sent -> Confirmed               by confirm
        Confirmed -> Locked                     by lock
        Draft | Sent | Confirmed -> Cancelled   by cancel  unless deliveries.any(t => t.state is Done)

    action send:
        require lines.count() > 0, "A quotation needs lines"
        emit SendEmail(to=customer.email ?? fail "The customer has no email", template="quotation", document=self)

    action confirm:
        require lines.count() > 0, "An order needs lines"
        confirmed_at = now()
```

## Flows

`flow field:` followed by one transition per line. The field must have an enum type, usually
an inline `enum(…)` with a default (the initial state).

| Transition | Meaning |
|---|---|
| `A -> B by act` | the action `act` moves the record from `A` to `B` |
| `A \| B -> C by act` | from any of the listed states |
| `A -> B by act when cond` | only if `cond` holds; several lines with the same action pick the first that applies |
| `A -> B by act unless cond` | refused while `cond` holds |
| `A -> B when cond` | **automatic**: the record moves as soon as `cond` holds |

```quadra
flow state:
    Draft -> Sent                                       by send
    Draft | Sent -> ToApprove                           by confirm  when needs_approval
    Draft | Sent -> Confirmed                           by confirm  when not needs_approval
    ToApprove -> Confirmed                              by approve
    ToApprove -> Draft                                  by reject
    Confirmed -> InProgress                             when work_orders.any(w => w.state is not Pending)
    Draft | Sent | ToApprove | Confirmed -> Cancelled   by cancel   unless receipts.any(t => t.state is Done)
```

An action named in a flow does not need an `action` block: if there is nothing else to do,
the transition is the whole action (`o.lock()` works with only the flow line above).

### Running a transition

When `record.act(…)` is called, in one transaction:

1. the permission to run `act` is checked (see [Security](/reference/security));
2. the `before Entity.act` reactions run;
3. the flow chooses the transition: the first line with action `act` whose source states
   contain the current state and whose `when` holds. If one exists but its `unless` holds, the
   action fails: `SalesOrder.cancel is not allowed: deliveries.any(t => t.state is Done)`.
   If the action has transitions but none applies:
   `SalesOrder.confirm is not allowed in state Locked`;
4. the body of the `action` block runs, if there is one;
5. the field takes the new state, and the `on Entity enters State` reactions run.

`when` and `unless` are evaluated before the body runs.

Automatic transitions (without `by`) are applied after each statement, together with the
other automations, until nothing changes. A transition without `by` and without `when` never
happens.

### Checks made by the compiler

| Message | Cause |
|---|---|
| `` `Z` is not a state of `state` `` | a state that is not a variant of the field |
| `` `s` must have an inline enum type to be used in a flow: declare it as `s: enum(Draft, Posted) default Draft` `` | the field is not declared with `enum(…)` (a named `enum` declaration is not accepted for a flow) |
| warning: `` state `Archived` of `Order.state` can never be reached `` | no chain of transitions leads to it from the initial state |

### The state changes only through the flow

The field of a flow is changed by its transitions, and by nothing else. Assigning it is a
compile error in actions, reactions, functions, fixtures and tests:

```text
error: `SalesOrder.state` changes only through its flow: run one of its actions (send, confirm, lock, cancel)
```

A flow with only automatic transitions says `it moves by its automatic transitions`. A new
record starts in the initial state (the field's default): `SalesOrder(state=Confirmed)` is
refused with `` `SalesOrder.state` changes only through its flow: a new SalesOrder starts in
Draft ``. At run time the same messages refuse an `update` of the API (`422`) and any other
path; the web client and the API do not offer the field for editing.

Migrations and [changes](/reference/migrations-and-changes) may set it: they are data fixes
(a state imported from another system, an order confirmed by mistake), run by the system with
a reason and an approval, and the rules still hold at their end (`frozen when` included: a
record set back to a state where the condition is false is unfrozen).

### Frozen states

A [`frozen when`](/reference/rules#frozen) condition usually names states of the flow. A
transition into those states freezes the record at the end of the transaction; a transition
out of them (`Confirmed -> Draft by reopen`) unfreezes it at once, so the body of `reopen`
and the `on … enters Draft` reactions can write the record and its lines.

### States computed from the lines

A state that only summarizes the lines is a formula, not a flow:

```quadra
state: enum(Empty, Cancelled, Done, Ready, Waiting) = when:
    moves.count() == 0                                    => Empty
    moves.all(m => m.state is Cancelled)                  => Cancelled
    moves.all(m => m.state in {Done, Cancelled})          => Done
    moves.all(m => m.state in {Ready, Done, Cancelled})   => Ready
    else                                                  => Waiting
```

## Actions

```quadra
action reverse(on: Date = today()) -> JournalEntry:
    require state is Posted and not reversed, "Only a posted entry can be reversed, once"
    let r = JournalEntry(journal=journal, date=on, source=source, reversal_of=self,
        lines=lines.map(l => JournalLine(account=l.account, partner=l.partner,
            currency=l.currency, amount_currency=-l.amount_currency, balance=-l.balance)))
    r.post()
    return r
```

`action name(param: T = default, …) -> ReturnType:` followed by a block of
[statements](/reference/expressions#statements). Parameters and the return type are
optional.

| In an action | |
|---|---|
| Fields | read and assigned by their bare names (`confirmed_at = now()`) or through `self` |
| Other records | created with constructors, changed by assignment, their actions called |
| `require cond, "message"` | fails the action when `cond` is false |
| `fail "message"` | fails the action |
| `return value` | the action's result |
| `emit Event(…)` | queues an event |

Call an action like a method: `so.confirm()`, `inv.credit_note(reason="Returned goods")`,
`entry.reverse(2026-12-31)`. The call is part of the current transaction: if anything fails
afterwards, everything the action did is undone. From the API, actions are run with the
`action` operation of [`POST /v1/tx`](/reference/http-api#writing).

The permission to run the action is checked when a user calls it; what the action does
inside (writes, other actions, records it creates) is not checked again: the action is the
unit of permission.

## Events

```quadra
event SendEmail(to: Email, template: Text, document: Document)

action send:
    emit SendEmail(to=customer.email ?? fail "The customer has no email", template="quotation", document=self)
```

`event Name(field: Type, …)` declares a typed message; `emit Name(field=value, …)` queues
one, its content computed there, in the transaction. Where it goes is a separate
declaration, [`outbound`](/reference/connections#how-an-event-goes-out). Arguments are named; an unknown one is an error: `` event `SendEmail` has no field `subject` ``.

### The outbox

Emitted events are collected in the transaction's outbox and dropped if the transaction is
undone, so nothing leaves the system for a transaction that did not happen. With a database,
the events of a transaction are written to the system table `_quadra_outbox` **in the same
database transaction as its commit**: an event exists if and only if its transaction was
committed.

| Column | Value |
|---|---|
| `id` | the event's number, in the order of `emit` |
| `tx` | the transaction that emitted it |
| `event` | the event's name (`SendEmail`) |
| `fields` | its fields as a JSON object, values encoded as in the history (`{"to": "anna@example.com", "document": {"ent": "SalesOrder", "ref": 71}}`) |
| `created_at` | the clock of the transaction |
| `delivered`, `delivered_at` | `false` / empty until it is delivered |
| `attempts`, `next_at`, `error`, `failed_at` | the delivery attempts of an event with an `outbound`, the time of the next, the last error, and when it failed for good |

An event with an [`outbound`](/reference/connections#outbound) is delivered by the server
through a connection of `quadra.toml`: a signed webhook or an email, tried again when the
receiver is unavailable:

```quadra
outbound SendEmail via mail
```

The others are left to an integration outside Quadra: a program that takes them through the
API with a lease and settles each ([`/v1/outbox`](/reference/connections#without-outbound-an-integration-takes-them)),
or, on the machine of the database, the CLI, which reads the pending events and marks them
delivered:

```bash
quadra outbox list --pending --json      # one JSON object per line, oldest first
quadra outbox delivered 41 42            # after they were handed on
```

Delivery is **at least once**: an integration that stops between handing an event on and
marking it delivered sends it again at the next run, so the receiver should ignore an `id`
it has already seen. In memory (tests, the sandbox) there is no outbox table: the events are
dropped after the commit. See [`quadra outbox`](/reference/cli#events-quadra-outbox).

## Reactions

Reactions are code that runs by itself, in the same transaction as what triggered it. (The
reactions to the delivery of an event, `on OrderShipped delivered as d:` and `on … failed as
f:`, run in a transaction of their own when the delivery ends: see
[Variables and connections](/reference/connections#reacting-to-the-outcome).)

| Declaration | Runs |
|---|---|
| `on Entity enters State as x:` | right after a record moves into `State` (by an action or automatically) |
| `on Entity becomes cond as x:` | when `cond` turns from false to true for a record |
| `on Entity created as x:` | once for each record created in the transaction, at its end |
| `on Entity changes as x:` | once for each existing record whose stored fields were written in the transaction, at its end |
| `before Entity.action as x:` | before the action, so it can add preconditions |

The record is bound to the name after `as`.

```quadra
# sales.qd: confirming an order creates its delivery
on SalesOrder enters Confirmed as o:
    let goods = o.lines.where(l => l.product.type is not Service)
    if goods.any():
        Transfer(kind=Delivery, warehouse=o.warehouse, partner=o.customer, origin=o,
            moves=goods.map(l => StockMove(product=l.product, qty=l.qty, uom=l.uom,
                source=o.warehouse.stock, dest=virtual_location(Customer), origin=o, sale_line=l)))

# invoicing.qd: a precondition added by a higher module
before SalesOrder.confirm as o:
    if let limit = o.customer.as_customer?.credit_limit:
        let limit_company = convert(limit, to=o.company.currency, on=o.date).round()
        let exposure = credit_exposure(o.customer, o.company, o.date) + o.total_company
        require exposure <= limit_company, f"Credit limit exceeded: exposure {exposure}, limit {limit_company}"

# planning.qd: reorder when the forecast drops below the minimum
on ReorderRule becomes to_order > 0 as r converges:
    match r.route:
        Buy =>
            let po = PurchaseOrder.find(p => p.vendor == r.vendor and p.warehouse == r.warehouse and p.state is Draft)
                     ?? PurchaseOrder(vendor=r.vendor, warehouse=r.warehouse, notes="Automatic reorder")
            PurchaseLine(order=po, product=r.product, uom=r.product.purchase_uom,
                         qty=r.to_order.to(r.product.purchase_uom).round_up())
        Make =>
            ProductionOrder(product=r.product, qty=r.to_order, warehouse=r.warehouse, due=today() + 7 days)
```

| Rule | Detail |
|---|---|
| Permissions | Reactions run as the system: their writes are not permission-checked. |
| Failure | A `require` or `fail` in a reaction fails the transaction that triggered it, so a failed `before` reaction stops the action. |
| `becomes` | Evaluated after each statement on the records the statement may have affected. A record created with the condition already true counts as a change from false. It does not fire again until the condition has been false. |
| `created`, `changes` | They run when the transaction's other automations (automatic transitions, `becomes`, postings) are stable, in the same transaction, once per record: a `changes` reaction that writes its own record does not run again for that write, and a record created in the transaction counts as created, not changed. On a trait (`on Document created`) they run for every implementing entity. There is no `deleted`: records are not deleted in Quadra (`on X deleted` is an error). |
| `converges` | Required on a `becomes` reaction that can trigger itself again: one that writes something its condition reads (the reorder above creates purchase lines, which `to_order` reads). Without it the compiler says `` `on ReorderRule becomes to_order > 0` can trigger itself again: it writes `PurchaseLine.qty`, … which its condition reads ``. It states that each run brings the condition back to false; the engine runs a `becomes` reaction at most once per record in a transaction, so it cannot loop. Only `becomes` reactions take it. |
| Termination | Automatic transitions, `becomes` reactions and ledger postings repeat until nothing changes; after 20 rounds the transaction fails with `the automations do not converge (more than 20 rounds)`. |
| Cause | Everything a reaction writes records it as the cause (`on SalesOrder enters Confirmed`), visible with `why(record)` and in the history. |

The name after `before` must be an action of the entity (`` `A` has no member `go` ``); the
state after `enters` must be one of its states (`` `A` has no state `Done` ``).

```quadra
on Invoice created as i:
    emit InvoiceCreated(invoice=i)

on Partner changes as p:
    PartnerReview(partner=p, due=today() + 7 days)     # every change is reviewed
```

## The cause of every change

Every write is recorded with its cause: the user, the action or reaction, the test
statement or API request, the change or migration, and the chain that led to it. `why(record)`
shows the chain as text:

```text
P P#22 created
  because P.n changed by test statement at main.qd:15  (system, 2026-01-01 09:00)
```

The [history endpoint](/reference/http-api#reading) of the API returns the same information
for every field.
