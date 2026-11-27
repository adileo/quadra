# Variables and connections

A project talks to the outside through what it declares in `quadra.toml`: **variables**,
whose values depend on where the app runs (an address, a rate, a token), and **connections**,
the systems it sends to (an HTTP endpoint, an SMTP server). The code names them; it never
holds a value that belongs to an environment, and it never reads a secret.

```toml
[vars]
SHOP_NAME = { default = "Acme" }
MAX_LINES = { type = "Integer", default = "50", test = "5" }
SHOP_TOKEN = { secret = true, description = "the API token of the shop" }
HOOK_SECRET = { secret = true }
SMTP_PASSWORD = { secret = true }

[connections.shop]
kind = "http"
url = "https://shop.example.com/api"
bearer = { var = "SHOP_TOKEN" }
signing_secret = { var = "HOOK_SECRET" }

[connections.mail]
kind = "smtp"
url = "smtps://smtp.example.com:465"
user = "noreply@example.com"
password = { var = "SMTP_PASSWORD" }
from = "Acme <noreply@example.com>"
```

```quadra
entity Order:
    number: Text
    customer_email: Email
    note: Text?

    action ship():
        note = f"shipped by {env.SHOP_NAME}"
        emit OrderShipped(number=number)
        emit SendEmail(to=customer_email, subject=f"Order {number}", body="On its way")

event OrderShipped(number: Text)
event SendEmail(to: Email, subject: Text, body: Text)

outbound OrderShipped via shop path "/hooks/orders"
outbound SendEmail via mail
```

## How an event goes out

Three declarations, each with one job:

| Declaration | Where | What it decides |
|---|---|---|
| `event OrderShipped(number: Text)` | the code | the **shape**: the fields every event of this kind carries, with their types |
| `emit OrderShipped(number=number)` | an action or a reaction | the **content**: the values of those fields, computed from the data in the transaction that emits it |
| `outbound OrderShipped via shop path "/hooks/orders"` | the code, naming a connection of `quadra.toml` | the **delivery**: where it goes and how (the url, the credentials, the signature), after the commit |

The content is made once, at `emit`, and written to the outbox with the commit: what goes out
is exactly what the transaction decided, even if the data changes before the delivery or an
attempt is repeated a day later. `outbound` does not compute anything: it turns the event's
fields into the message the connection sends. Through HTTP, the fields become the `data` of
the JSON body ([Webhooks](#webhooks)); through SMTP, the event's `to`, `subject` and `body`
become the email ([Emails](#emails)). To send something different, emit different values:

```quadra
entity Shipment:
    order: Text
    carrier: Text
    weight: Decimal

    action dispatch():
        emit ShipmentReady(order=order, carrier=carrier, weight_kg=weight, label=f"{order}/{carrier}")

event ShipmentReady(order: Text, carrier: Text, weight_kg: Decimal, label: Text)
```

```json
{"id": 12, "event": "ShipmentReady", "tx": 88, "created_at": "2026-11-27T10:14:03Z", "data": {"order": "A-1042", "carrier": "DHL", "weight_kg": "2.5", "label": "A-1042/DHL"}}
```

The envelope (`id`, `event`, `tx`, `created_at`) and the field names are fixed today. A body in
the format a third party imposes (its own field names, nesting, a different path per event)
will come with `outbound … as e:`, a block that builds the message from the event, still
without reading the database.

## Variables

Each key of `[vars]` is a variable, in capitals, digits and `_` (`SHOP_TOKEN`):

| Key | Meaning |
|---|---|
| `type` | `Text` (when not given), `Integer`, `Decimal` or `Boolean`: the values are checked against it, and `env.NAME` has that type |
| `default` | the value where the environment gives none. A variable without a default must have a value wherever the app runs |
| `secret` | `true`: a credential. The code cannot read it; only a connection uses it. It has no default, since `quadra.toml` is in the repository |
| `test` | the value in the project's tests (see [Testing](#in-tests)); without it, the default |
| `description` | shown by the editor next to `env.NAME` |

### `env.NAME`

The code reads a variable that is not secret as `env.NAME`:

```quadra
action ship():
    note = f"shipped by {env.SHOP_NAME}"
```

| Error | Why |
|---|---|
| `` `env.NOPE`: quadra.toml declares no variable NOPE ([vars]) `` | every variable is declared |
| `SHOP_TOKEN is a secret: the code cannot read it (a connection of quadra.toml uses it for its credentials)` | a secret goes only to a connection: it cannot end up in a field, an event or a log |
| `` `env.SHOP_NAME` is not allowed here: its value depends on where the app runs, … `` | formulas, checks, functions and constants must give the same result in every environment and when the history is replayed. Read variables in actions, reactions and scripts, where what they produce is written to the history |

### Where the values come from

| Where the app runs | Values |
|---|---|
| `quadra serve` | the process's environment, then a `.env` file at the project root for what the environment does not have. A variable without a default and without a value stops it from starting: `error: a variable is not set: SHOP_TOKEN (declared in quadra.toml, [vars], without a default)` |
| `quadra dev` | the same; a missing value is a warning, and the events with an `outbound` wait in the outbox |
| `quadra test` | the `test` values, else the defaults: never the environment |
| Quadra Cloud | the values of the environment ([`quadra cloud env`](#on-quadra-cloud)); previews and forks do not inherit production's |

A `.env` file holds `NAME=value` lines (`#` starts a comment, quotes are removed). Keep it out
of the repository.

## Connections

`[connections.<name>]`, the name in lower case letters, digits and `_`. Each setting is
written as text or taken from a variable with `{ var = "NAME" }`; a credential (`bearer`,
`api_key`, `password`, `signing_secret`) must come from a **secret** variable:

```text
quadra.toml, [connections.shop]: bearer is a credential: not written in quadra.toml (it is in the repository) but `{ var = "NAME" }`, a secret variable
```

### `kind = "http"`

| Key | Meaning |
|---|---|
| `url` | the base address, `https://` (plain `http://` only with `insecure = true`) |
| `bearer` | sends `Authorization: Bearer <value>` |
| `api_key`, `api_key_header` | sends the key in a header (`X-Api-Key` when not given) |
| `user`, `password` | HTTP basic authentication |
| `signing_secret` | signs every request (see [Webhooks](#webhooks)) |
| `timeout` | from `"1s"` to `"120s"`; `"10s"` when not given |
| `insecure` | `true` allows `http://`: for a test receiver on your machine |

### Limits of the other side

Both kinds take what the system on the other side accepts, per tenant:

| Key | Meaning |
|---|---|
| `rate` | at most so many requests: `"2/s"`, `"100/min"`, `"1000/h"` (none when not given) |
| `concurrency` | requests at once, from 1 to 64 (4 when not given) |
| `pause_after` | failures of transport in a row (no answer, a 5xx) before the connection is paused (5 when not given); a 401 or 403 (credentials refused) pauses it at once |

While a connection is paused its events wait, without costing attempts, for 30 seconds, then
twice as long each time up to an hour; then one event goes alone, as a probe, and the first
answer resumes the others. (The pause is kept in memory: a restart starts afresh.)

### `kind = "smtp"`

| Key | Meaning |
|---|---|
| `url` | `smtps://host:465` (TLS from the start) or `smtp://host:587` (STARTTLS, required) |
| `user`, `password` | the SMTP login |
| `from` | the sender, `Name <address>` or an address |
| `insecure` | `true` allows `smtp://` without TLS: for a local test server such as Mailpit |

## `outbound`

```quadra
outbound OrderShipped via shop path "/hooks/orders"
outbound SendEmail via mail
```

`outbound Event via connection` sends every event of that kind through the connection,
**after its transaction has committed** and the commit is confirmed where it is kept (on
Quadra Cloud, as the plan's durability says): an event emitted by a transaction that is undone,
or whose commit could still be lost, never leaves. An event has at most one `outbound`.

| Error | Why |
|---|---|
| `` no event `Nope` (declare it with `event Nope(…)`) `` | |
| `` no connection `ftp` in quadra.toml ([connections.ftp]) `` | |
| `` `OrderShipped` already has an outbound: an event goes out by one connection `` | |
| ``an email event has `to`, `subject` and `body`: `Bad` has no `subject` `` | an SMTP connection sends the event's `to`, `subject` and `body` (Text or Email) |
| ``the path `x` is a path of the connection's url: from `/`, no spaces, no `..` `` | |

### The request: `outbound … as e:`

Without a block, an event goes out as the webhook below (or, by SMTP, as the email of its `to`,
`subject` and `body`). With a block, the code builds the request in the shape the receiver
wants, from the event and the data:

```quadra
record ErpShipment from json:
    order: Text at "order.number"
    weight: Decimal at "parcel.weight_kg"

record ErpAck from json:
    tracking: Text

event OrderPlaced(order: Order)

outbound OrderPlaced via erp as e:
    reply ErpAck
    let o = e.order
    put(f"/shipments/{o.number}", ErpShipment(order=o.number, weight=o.weight))

on OrderPlaced delivered as d:
    d.event.order.note = d.reply?.tracking ?? "no tracking"
```

| Rule | Detail |
|---|---|
| `e` | the event: its fields, as they were emitted |
| The data | read **as it was when the event was emitted**: every attempt builds the same request, even a day later and after the order changed. The content is decided at `emit`; the block decides its shape |
| No writes | the block builds the request and nothing else: an assignment or an `emit` there is an error (write in the transaction that emits, or in `on … delivered`) |
| The request | the block ends with it. HTTP: `post(path, body)`, `put(path, body)`, `patch(path, body)`, `delete(path)`; the path is appended to the connection's url (and to the outbound's `path`), the body is a record `from json` (sent as `application/json`), a record `from xml` (sent as `application/xml`) or text. SMTP: `email(to=…, subject=…, body=…, html=…)` (with `html`, the email has both versions) |
| `reply T` | the first line of the block: the answer of a 2xx, read as a record `from json` or `from xml`, is `d.reply` in `on … delivered` (none when it does not fit, said in the log) |
| A block that fails | (a bug, a record that no longer exists) the event waits, without a time limit, for a deploy that fixes it; the error is in its log |

The headers, the authentication, the signature and the `Idempotency-Key` are the connection's,
as for a webhook.

### The policy: how long, when, in what order

The first lines of an `outbound … as e:` block say how it is delivered. They are all optional;
a block may have only them (then the request is the usual webhook or email):

```quadra
outbound ShippedEmail via mail as e:
    expires after 1 days                          # an email a day late is not worth sending
    unless e.order.state is Cancelled             # nor one about an order cancelled meanwhile
    window "08:00" .. "20:00"                     # and not at night (the project's zone)
    email(to=e.order.customer.email, subject=f"Order {e.order.number} shipped", body="On its way")

outbound StockChanged via shop as e:
    latest by e.product                           # only the newest stock of a product goes
    retry for 3 days
    backoff 10 seconds .. 10 minutes
    put(f"/inventory/{e.product.code}", ShopStock(available=e.available))

outbound LedgerPosted via erp as e:
    ordered by e.company                          # one at a time per company, in order
    key f"posting-{e.posting.number}"
    post("/postings", ErpPosting(number=e.posting.number, amount=e.posting.amount))
```

| Line | Effect |
|---|---|
| `retry for D` | tried again for `D` after the emit (a day when not said), then failed for good |
| `retry N times` | at most `N` attempts |
| `backoff A .. B` | the wait after a failed attempt: `A`, doubled each time up to `B` (30 seconds .. 1 hour when not said) |
| `expires after D` | not sent once `D` has passed since the emit: `expired` |
| `unless C` | read before each attempt **on the data as it is now**; true: not sent, `skipped`, with the condition as the reason |
| `delay D` | not sent before `D` after the emit |
| `window "HH:MM" .. "HH:MM"` | sent only between those times of day, in the project's zone (across midnight when the first is later) |
| `key K` | the `Idempotency-Key`: text of your own (`f"ship-{e.order.number}"`), the same for the same thing even if emitted twice |
| `ordered by K` | one at a time per value of `K`, in the order emitted: the next waits for the outcome of the one before. One that failed for good holds its value back (not the others) until it is retried or skipped, since skipping it silently would break the sequence |
| `latest by K` | an event still waiting is `superseded` by a newer one with the same value: only the newest goes (a stock level, a price) |

Without `ordered by`, events are sent in parallel and their order is not guaranteed. The
durations are fixed (`3 days`); `unless`, `key`, `ordered by` and `latest by` are read for each
event, the last three on the data as it was at the emit. Every decision (`expired`, `skipped`,
`superseded`, a wait) is in the event's log.

### Batches

An API that takes many items in one call gets them in batches:

```quadra
outbound StockChanged via shop path "/inventory/bulk" batch 50 within 30 seconds as e:
    latest by e.product
    post("", ShopBulk(levels=e.map(x => ShopLevel(sku=x.product.code, available=x.available))))
```

| Rule | Detail |
|---|---|
| When | a batch goes once it has `50` events, or once its oldest has waited `30 seconds` (without `within`, at once with what is there) |
| `e` | in the lines of policy, each event (they decide which events wait, expire or are superseded); in the code, the list of the batch's events, oldest first |
| The data | read as it was at the last of the batch's emits |
| The outcome | the same for each of its events: each is delivered or failed, with its own reactions; the `Idempotency-Key` is the batch's (`quadra-batch-…`), the same at every attempt |
| Not with | `ordered by`, `key`, `reply`: a batch's events go together |

### `publish`: keeping a record in step with another system

When another system must always have the current state of a kind of record (the products of a
shop, the customers of a CRM), there is no event to write: `publish` says which records go and
how, and Quadra sends a record each time it is created or changes.

```quadra
publish Product to shop as p:
    when p.sellable                               # only these records
    on price, stock, name                         # only a change of these fields sends it again
    debounce 30 seconds                           # many changes in a row: only the last state goes
    put(f"/products/{p.sku}", ShopProduct(sku=p.sku, price=p.price, available=p.stock))
```

| Line | Effect |
|---|---|
| `when C` | the record is sent only when `C` holds after the change (without it, every record) |
| `on f1, f2` | a change sends it only when one of these fields changed (without it, any change); a new record always |
| `debounce D` | it waits `D` after the change; a newer change of the same record in the meantime supersedes it |
| the code | builds the request from `p`, the record as it was at the change, like the block of an `outbound` |

What it means is an `outbound`: Quadra makes an event per record (`PublishProductToShop`, with
the field `record`), emits it when the record is created or changes, and delivers it with
`latest by` the record and `delay` the debounce. So everything about an outbound holds:
attempts, the log, retry and skip, the policy lines of [above](#the-policy-how-long-when-in-what-order)
(`retry for`, `window`, `expires after`), and the reactions (`on PublishProductToShop failed as
d:`).

In the lines of policy and the code, `p` is the record (and the event's other name is not
needed). The event is named after the record and the connection, in capitals:
`publish Product to erp_eu` makes `PublishProductToErpEu`.

A change that came from the same connection is not sent back: when an `inbound` of `shop`
changes a product's stock, or a reaction to the outcome of a delivery to `shop` writes the
product, the product is not published to `shop` again (no echo between two systems that keep
each other in step). A change made by anyone else, or through another connection, is.

### The other system's ids

Another system has its own id for each record. Keep it in a field of the record, one per
system, set from its answer:

```quadra
entity Product:
    sku: Text unique
    price: Decimal
    shop_id: Text?                                # the id the shop gave it

record ShopProduct from json:
    sku: Text
    price: Decimal

record ShopCreated from json:
    id: Text

publish Product to shop as p:
    reply ShopCreated
    if let id = p.shop_id:
        put(f"/products/{id}", ShopProduct(sku=p.sku, price=p.price))
    else:
        post("/products", ShopProduct(sku=p.sku, price=p.price))

on PublishProductToShop delivered as d:
    d.event.record.shop_id = d.event.record.shop_id ?? d.reply?.id
```

The first delivery creates the product there and keeps its id; the next ones update it. What
the shop sends in is matched the same way (`Product.where(x => x.shop_id == m.id).first()`),
and writing the id is not sent back to the shop. To create a product only once even if the first
answer is lost, give the shop a key of your own (`key f"product-{p.sku}"`) when it accepts one.

### Webhooks

Without a block, through an HTTP connection an event is a `POST` of JSON to the connection's
`url` followed by the `path`:

```json
{"id": 41, "event": "OrderShipped", "tx": 1207, "created_at": "2026-11-27T10:14:03Z", "data": {"number": "A-1042"}}
```

`data` holds the event's fields as plain JSON:

| Value | In `data` |
|---|---|
| `Text`, `Email`, … | a string |
| `Int`, `Bool` | a number, `true` / `false` |
| `Decimal`, a percentage | a string, exact: `"2.50"`, `"0.22"` |
| `Date`, `DateTime` | `"2026-11-27"`, `"2026-11-27T10:14:03Z"` (UTC) |
| an amount, a quantity | `{"amount": "12.50", "currency": "EUR"}`, `{"quantity": "2", "unit": "kg"}` |
| a record | `{"entity": "Order", "id": 71}` |
| a list, a set | an array |
| an enum value | its name: `"Shipped"` |
| none | `null` |

The headers:

| Header | Value |
|---|---|
| `Quadra-Event` | the event's name |
| `Quadra-Delivery` | its `id` |
| `Idempotency-Key` | `quadra-<commit>-<id>` (the first 16 characters of the hash of the commit that emitted it, and its `id`): the same on every attempt of the same event, never the same for two events |
| `Quadra-Signature` | with a `signing_secret`: `t=<unix seconds>,v1=<hex HMAC-SHA256 of "<t>.<body>">` |

A receiver checks the signature by computing the HMAC of the timestamp, a dot and the raw
body with the shared secret, comparing it in constant time, and refusing a timestamp more than
a few minutes old:

```python
import hmac, hashlib, time

def verify(secret: bytes, body: bytes, header: str) -> bool:
    parts = dict(p.split("=", 1) for p in header.split(","))
    t = parts["t"]
    if abs(time.time() - int(t)) > 300:
        return False
    expected = hmac.new(secret, f"{t}.".encode() + body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, parts["v1"])
```

### Emails

Through an SMTP connection an event is a plain-text email from the connection's `from` to the
event's `to`, with its `subject` and `body`.

### Reacting to the outcome

When a delivery ends, delivered or failed for good, the code can react to it in a transaction
of its own, as the system, with the cause `outbound:Event#id delivered` (or `failed`): the
outcome is then in the history like any other write.

```quadra
entity Order:
    number: Text
    note: Text?

event OrderPlaced(order: Order, number: Text)

on Order created as o:
    emit OrderPlaced(order=o, number=o.number)

outbound OrderPlaced via shop path "/orders"

on OrderPlaced delivered as d:
    d.event.order.note = f"sent: {d.status ?? 0} after {d.attempts} attempt(s)"

on OrderPlaced failed as f:
    f.event.order.note = f"not sent: {f.error}"
```

| In the reaction | Value |
|---|---|
| `d.event` | the event's fields, as they were emitted (`d.event.order`, `d.event.number`) |
| `d.reply` | with `reply T` on the outbound: the answer, read as a `T` (optional) |
| `d.at` | when the outcome came |
| `d.attempts` | the attempts made, this one included |
| `d.status` | the HTTP status or the SMTP reply code (optional) |
| `d.response` | the beginning of the answer (optional) |
| `f.error` | for `failed` only: why |

The event must have an `outbound`: `` `Orphan` has no `outbound`: nothing delivers it, so it is
never delivered or failed ``. A reaction that fails does not change the outcome (the receiver
got the event, or not): the error goes to the event's log (`quadra outbox log`), as a step
`reaction`.

### Attempts and failures

| Answer | What happens |
|---|---|
| HTTP 2xx, SMTP accepted | delivered |
| HTTP 408, 429 or 5xx, a timeout, no connection, SMTP 4xx | tried again: after 30 s, then twice as long each time up to an hour, for a day; a 429 or 503 with `Retry-After` (in seconds) waits at least that long, up to an hour |
| HTTP 401 or 403 | tried again in the same way: the connection's credentials were refused, and a renewed token should not cost the events |
| any other HTTP answer (a 4xx, a redirect: redirects are not followed), SMTP 5xx | failed for good |
| a day of failed attempts | failed for good |
| a value missing where the app runs (a variable the connection needs) | waits, without a time limit, until the configuration is fixed |

Each event has a state: `pending` (waiting for its next attempt), `delivered`, `failed` (for
good), `skipped` (by `unless`, or by hand with a reason), `expired` (`expires after`) or
`superseded` (`latest by`). Every attempt is written to the event's **log**
with the date and time, the connection, how long it took, the HTTP status or SMTP reply, the
error and the beginning of the answer; so is every `retry` and `skip`, with who asked and why.
The log is kept with the data (on Quadra Cloud, in the bucket like any write) and never
changed:

```bash
quadra outbox list --state failed
quadra outbox log 41
quadra outbox retry 41                                  # failed or skipped: pending again
quadra outbox skip 42 --reason "the order was cancelled"
```

```text
#41  2026-11-27T10:14:05  attempt  pending     via shop  503  412 ms  shop.example.com answered 503: busy
#41  2026-11-27T10:14:36  attempt  pending     via shop  503  388 ms  shop.example.com answered 503: busy
#41  2026-11-27T10:15:41  attempt  delivered   via shop  200  97 ms
        {"ok":true}
```

([CLI](/reference/cli#events-quadra-outbox)). Delivery is **at least once**: if the
server stops between sending and recording the answer, the event is sent again with the same
`Idempotency-Key`, which the receiver should use to ignore what it has already seen.

On Quadra Cloud connections only reach public addresses: a name that resolves to a private,
loopback or link-local address (the clouds' metadata, `169.254.169.254`) is refused, and so
is a redirect. Previews and forks deliver nothing ([On Quadra Cloud](#on-quadra-cloud)).

## `inbound`: messages in

A system that sends messages (a shop's webhooks, a cash register, a carrier's status updates)
reaches an `inbound`. Each message is kept raw, then handled by code in a transaction of its
own:

```toml
[vars]
SHOP_WEBHOOK_SECRET = { secret = true }

[connections.shop]
kind = "http"
url = "https://acme.myshopify.com/admin/api"
inbound_signature = "shopify"
inbound_secret = { var = "SHOP_WEBHOOK_SECRET" }
```

```quadra
record ShopOrder from json:
    id: Text
    number: Text at "name"
    total: Decimal at "total_price"

inbound OrderIn via shop path "/orders" as m: ShopOrder:
    key m.id
    if m.total < 0:
        fail "a negative total"
    let o = Order(number=m.number, total=m.total)
```

The shop sends to `POST https://<app>/in/shop/orders` (`/in/<connection>` and the inbound's
`path`). What happens to a message:

| Step | Rule |
|---|---|
| Who sent it | checked first, by the connection's `inbound_signature`; a message refused (a wrong signature, too large, no inbound at that path) never touches the data and is in the log |
| Kept | raw, with its hash, **before** it is handled; the sender gets its `202` only once it is kept where the durability rule wants it |
| Duplicates | a message whose `key` was received before is not kept again (`200` with `"duplicate": true`); the same key with another body is a conflict (`409`), in the log. Without `key`, the hash of the body tells two messages apart |
| Read | as its record (`ShopOrder`); one that does not fit is kept `rejected`, with the error, to be handled again by a deploy that reads it |
| Handled | the code after the policy lines, as in an action, in a transaction with the cause `inbound:OrderIn#12 handled`; one that fails (`fail`, a rule broken) is `rejected` with the error, and the next messages go on |
| Once | a message is handled once, even if the server stops between its transaction and writing it |

| `inbound_signature` | What the sender adds, with `inbound_secret` |
|---|---|
| `quadra` | `Quadra-Signature: t=…,v1=…` (Quadra's own webhooks: [Webhooks](#webhooks)) |
| `stripe` | `Stripe-Signature: t=…,v1=…` |
| `hmac` | `X-Signature: <hex HMAC-SHA256 of the body>` |
| `shopify` | `X-Shopify-Hmac-Sha256: <base64 HMAC-SHA256 of the body>` |
| `github` | `X-Hub-Signature-256: sha256=<hex>` |
| `token` | `Authorization: Bearer <the secret>` |

A timestamped signature more than five minutes old is refused. `inbound_secret_previous` is
also accepted while the sender switches to a new secret; `inbound_max_kb` is the largest body
(1024 when not given).

The messages, their states (`received`, `done`, `rejected`, `skipped`) and the log of each,
with the same commands as the outbox:

```bash
quadra inbox list --state rejected
quadra inbox log 12
quadra inbox retry 12                     # handled again by the code deployed now
quadra inbox skip 13 --reason "a test order from the shop"
```

### Pulled instead of sent

An API that sends nothing is asked, at intervals, for what changed since the last time:

```quadra
record ShopOrders from json:
    orders: [ShopOrder]

inbound OrderPulled via shop as m: ShopOrder:
    pull "/orders.json?updated_at_min={cursor}" every 5 minutes
    items ShopOrders.orders
    cursor m.updated_at
    key m.id
    let o = Order(number=m.number, total=m.total)
```

| Line | Effect |
|---|---|
| `pull "path" every D` | a `GET` of the connection's url and `path` every `D` (a minute at least), with the connection's authentication; `{cursor}` in the path is replaced by the cursor (empty the first time), encoded for a url |
| `items Page.field` | the answer is a `Page` and its `field` (a list of the inbound's record) holds the messages; without it the answer is that list |
| `cursor m.field` | of each message; the largest becomes the cursor, kept with the messages in one write, so that the next pull starts there. Ask for `>=` rather than `>`: what is seen twice is a duplicate of its `key`, what falls between two pulls is not lost |

Each message pulled is kept and handled as one sent in: the same states, duplicates, log
and commands. A pull that fails (no answer, not a 2xx, not JSON) is tried again within five
minutes, or after its `Retry-After`; the error is in the log of message 0 of the inbound. Pulls
run only where the app delivers its events (production on Quadra Cloud), and a sleeping app is
woken for the one due. A pull can sit next to webhooks on the same inbound (`path`), to catch
what a webhook missed.

## `endpoint`: answered at once

Some systems ask and wait for the answer: a cash register asks for a price, a checkout for the
stock. An `endpoint` answers them from the data, at `/in/<connection><path>`:

```quadra
record PriceQuery from json:
    sku: Text

record PosPrice from json:
    sku: Text
    price: Decimal

endpoint GET "/price" via pos as q: PriceQuery -> PosPrice:
    let p = Product.where(x => x.sku == q.sku).first() ?? fail "unknown sku"
    PosPrice(sku=p.sku, price=p.price)

record PosSale from json:
    uuid: Text
    sku: Text

record PosAck from json:
    sales: Int

endpoint POST "/sales" via pos as s: PosSale -> PosAck:
    key s.uuid
    let sale = Sale(uuid=s.uuid, sku=s.sku)
    PosAck(sales=Sale.where(x => x.sku == s.sku).count())
```

| Rule | Detail |
|---|---|
| Who | the caller is checked by the connection's `inbound_signature`, as for an `inbound` (for a cash register, `token`) |
| The request | `GET`: its query (`?sku=A-1`) read as the record; `POST`: its body |
| The answer | the block's last expression, a record `from json`, sent as JSON with `200`; a `fail` (or a broken rule) answers `422` with the message |
| `GET` | reads only: a write there is refused by the compiler |
| `POST` | a transaction of its own (cause `inbound:POST /sales#id handled`), kept in the inbox like a message; with `key`, the same request sent again (a register that did not hear the answer) gets the same answer and is not handled twice |
| No I/O | nothing goes out from the block; to tell another system, `emit` |

## Formats: `record … from`

The documents and files of another system (a shop's order, an e-invoice, a point of sale's
export, a bank's file) are described as records `from` their format: what a field is called
there, where it is, what type it has. Four formats are read and written:

| Format | Declared | Read | Written |
|---|---|---|---|
| JSON | `record ShopOrder from json:` | `ShopOrder.parse(text)` | `order.json()` |
| XML | `record Invoice from xml:` | `Invoice.parse(text)` | `invoice.xml()` |
| CSV | `record PosRow from csv(sep=";"):` | `PosRow.parse_rows(text)`, a `[PosRow]` | `rows.csv()` |
| Fixed-width | `record CbiRow from fixed:` | `CbiRow.parse_rows(text)`, a `[CbiRow]` | `rows.fixed()` |

Reading and writing are pure functions, the same text always giving the same records: they
can be used in functions and tests, and to build what an [`outbound`](#outbound) sends (a
record `from json` is sent as `application/json`, one `from xml` as `application/xml`; `reply`
reads either). A text that does not fit fails with where and why: the path in a document, the
line and the column in a file. Nothing is read from or written to disk: the text comes from
the code, and what is written is text.

### JSON

```quadra
record ShopLine from json:
    sku: Text
    quantity: Int
    price: Decimal

record ShopOrder from json:
    id: Text                                # a number too large for one: read as its text
    number: Text at "name"
    email: Email?
    total: Decimal at "total_price"         # "129.90" or 129.90
    placed: DateTime at "created_at"
    lines: [ShopLine] at "line_items"
    zip: Text at "shipping_address.zip"     # a path in nested objects

record ErpShipment from json:
    order: Text at "order.number"
    weight: Decimal at "parcel.weight_kg"
    note: Text?
```

```quadra
let o = ShopOrder.parse(text)             # fails with where and why: `ShopOrder.lines[0].quantity: an integer was expected, not "two"`
let s = ErpShipment(order=o.number, weight=2.5)
s.json()                                  # {"order":{"number":"#1001"},"parcel":{"weight_kg":2.5}}
```

| Rule | Detail |
|---|---|
| Keys | a field is under its name, or under `at "a.b.c"`; keys the record does not declare are ignored when reading |
| Types | `Text` (and `Code`, `Email`, `Phone`, `VatNumber`, `Barcode`): a string, or a number read as its text. `Int`, `Decimal` (written exactly; read from a number or a string), `Bool`, `Date` (`"2026-11-27"`), `DateTime` (`"2026-11-27T10:14:03Z"`; an instant without a zone is UTC), an enum (the variant's name), another record `from json`, `[T]`, `T?` (missing or `null`; left out when written) |

### XML

```quadra
record InvoiceLine from xml:
    ns cbc "urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2"
    ns cac "urn:oasis:names:specification:ubl:schema:xsd:CommonAggregateComponents-2"
    id: Int at "cbc:ID"
    quantity: Decimal at "cbc:InvoicedQuantity"
    unit: Code at "cbc:InvoicedQuantity/@unitCode"     # an attribute
    amount: Decimal at "cbc:LineExtensionAmount" decimals 2
    name: Text at "cac:Item/cbc:Name"

record Invoice from xml(root="Invoice"):
    ns "urn:oasis:names:specification:ubl:schema:xsd:Invoice-2"   # the default namespace
    ns cbc "urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2"
    ns cac "urn:oasis:names:specification:ubl:schema:xsd:CommonAggregateComponents-2"
    number: Text at "cbc:ID"
    issued: Date at "cbc:IssueDate"
    supplier: Text at "cac:AccountingSupplierParty/cac:Party/cac:PartyName/cbc:Name"
    note: Text? at "cbc:Note"
    total: Decimal at "cac:LegalMonetaryTotal/cbc:PayableAmount" decimals 2
    currency: Code at "cac:LegalMonetaryTotal/cbc:PayableAmount/@currencyID"
    lines: [InvoiceLine] at "cac:InvoiceLine"            # every such element, in order
```

```quadra
let inv = Invoice.parse(text)       # `Invoice/cac:InvoiceLine[2]/cbc:InvoicedQuantity: a number was expected, not `two``
inv.lines.sum(l => l.amount)
inv.xml()                           # <?xml version="1.0" encoding="UTF-8"?>\n<Invoice xmlns="urn:…" xmlns:cbc="urn:…" …><cbc:ID>…
```

| Rule | Detail |
|---|---|
| Paths | `at "a/b"`: elements separated by `/`; `at "a/@id"`: an attribute (the last step); `at "."`: the text of the record's own element; without `at`, the child element of the field's name |
| Namespaces | `ns prefix "uri"` lines at the top of the record bind the prefixes of its paths, `ns "uri"` the namespace of its unprefixed elements (an unprefixed attribute is in none). Each record declares the prefixes of its own paths, also one used inside another. Elements are matched by namespace and name, not by prefix: a document may use other prefixes |
| `root` | the name of the root element (`root="p:FatturaElettronica"`); when given, a document with another root is refused. Without it, any root is read and the record's name is written |
| Values | the text of an element (its text and CDATA, not its children) or an attribute; the types are those of JSON, written as text (`12.50`, `true`, `2026-11-27`). A record `from xml` is an element, `[T]` the repeated elements of the path, `T?` an element or attribute that may be missing (an empty element is none, except for text) |
| Writing | in the order of the fields (declare them in the order of the schema); fields share the steps of their paths (`a/b` and `a/c` are in one `a`), each item of a list repeats the last element, an optional without a value is left out; the namespaces are declared on the root, or where an element needs another |
| Safety | a DTD (`<!DOCTYPE …>`) is refused, so no entity is defined or expanded (no external entities, no "billion laughs"): only `&amp;` `&lt;` `&gt;` `&quot;` `&apos;` and character references. At most 10 MiB, 64 levels of elements, 200,000 elements |

### CSV

```quadra
enum Payment:
    Cash, Card, Voucher

record PosRow from csv(sep=";", decimal=",", header=true, encoding="latin1"):
    store: Code at "NEGOZIO"
    day: Date at "DATA" format "%d/%m/%Y"
    description: Text at "DESCRIZIONE"
    quantity: Int at "QTA"
    amount: Decimal at "IMPORTO" decimals 2
    payment: Payment at "PAGAMENTO"
    refund: Bool at "RESO" format "S/N"
    note: Text? at "NOTE"

record Movement from csv(header=false):
    account: Code at "1"            # without a header: the position, from 1
    amount: Decimal at "3"
    reference: Text? at "2"
```

```quadra
let rows = PosRow.parse_rows(text)  # `line 14, column `IMPORTO`: a number with `,` before the decimals was expected, not `3.50``
rows.where(r => not r.refund).sum(r => r.amount)
rows.csv()                          # NEGOZIO;DATA;DESCRIZIONE;…\r\nMI01;27/11/2026;"Penna; blu";…\r\n
```

| Rule | Detail |
|---|---|
| Options | `sep` (`","`; `"\t"` for tabs), `decimal` (the decimal separator, `"."`), `header` (`true`), `encoding` (`"utf-8"` or `"latin1"`, ISO-8859-1) |
| Columns | with a header, a field is the column of its name or of `at "NAME"`: in any order, the columns the record does not declare ignored, a column missing from the header an error (none for an optional field). Without a header, `at "3"` is the position, from 1; a field without `at` is at its place among the fields |
| Quoting | RFC 4180: a cell that holds the separator, a quote or a new line is between quotes, a quote inside doubled (`"say ""hi"""`). Rows end with `\r\n` when written (`\n` is read too); empty lines are skipped |
| Values | one per cell: the types of JSON but lists and records. Numbers use `decimal` (`3,50`, refused as `3.50`); an empty cell is none for an optional, the empty text for a `Text`, an error for the others; a row too short for a field is an error with its line and column |
| Formulas | a text cell that starts with `=`, `+`, `-`, `@` (or a tab or a carriage return) is written after a `'`, so that a spreadsheet shows it instead of running it; reading removes that `'` again. Numbers are never changed: `-3` stays `-3` |
| Encoding | the text a program reads has already been decoded; `encoding="latin1"` makes writing refuse a character latin1 cannot hold (`row 3, column `DESCRIZIONE`: `€` cannot be written in latin1`), and a host that receives the file as bytes decodes it with it (invalid UTF-8 says its line) |
| Empty | `rows.csv()` of no rows is the header alone |

### Fixed-width

The files of banks (CBI), of legacy systems and of AS/400: each field has its columns, from 1,
both included.

```quadra
record CbiRow from fixed:
    kind: Code at "1..2"
    account: Code at "3..14"
    value_date: Date at "15..20" format "%d%m%y"
    amount: Decimal at "21..33" decimals 2      # 0000000001250 is 12.50
    sign: Code at "34"                          # one column
    description: Text at "35..64"
    cro: Int? at "65..74"
```

```quadra
let rows = CbiRow.parse_rows(text)  # `line 3, columns 21..33 (`amount`): digits were expected, not `0000000012,50``
rows.fixed()                        # 611234567     2711260000000001250CFattura 118 …\r\n
```

| Rule | Detail |
|---|---|
| Columns | `at "11..20"` or `at "7"`, counted in characters; every field has them, and two fields may not overlap. The columns no field declares are spaces when written; a line shorter than the record reads as if it went on with spaces |
| Text | left-aligned, padded with spaces (the spaces after it are not part of it when read); a text longer than its columns is an error, never cut. Enums, dates (`format "%d%m%y"`, else ISO 8601) and booleans (`1` / `0`, or `format "S/N"`) are written the same way |
| Numbers | right-aligned, padded with zeros, the sign first (`-0042`). A `Decimal` says how many of its last digits are decimals, `decimals 2`, and no separator is written; a number that does not fit is an error |
| Empty | columns of spaces are none for an optional field, the empty text for a `Text`, an error for the others |
| Options | `encoding` (`"utf-8"` or `"latin1"`), as for CSV. Lines end with `\r\n` when written |
| Not yet | EBCDIC and packed decimals (COMP-3) are not read or written: such files are converted to text first. A file mixing records of several layouts is read one layout at a time |

### In every format

| Rule | Detail |
|---|---|
| `format "…"` | how a date or an instant is written (`format "%d/%m/%Y"`, chrono's patterns), or the text of true and false (`format "S/N"`). Refused on other types |
| `decimals N` | a `Decimal` is written with exactly `N` decimals (`12.50`); one with more is refused, not rounded (round it first). Refused on other types |
| Not in a format | a record of the database (`Customer`): send its id, or a record `from` the format made of its fields; amounts with a currency: a `Decimal` and a `Code`. A record inside one is `from` the same format |
| Refused | what the format cannot hold (a list in a CSV cell, a list of lists in XML), an `at` that is not in the syntax of the format, an unknown option, a prefix not declared, `parse` on a file of rows (`parse_rows`), and `at`, `format`, `decimals`, `ns` outside a record `from` a format |
| Only records | `entity X from json` is refused: what comes from outside has no identity until the code makes a record of it |
| Equality | a record read equals the same record built in code (`PosRow.parse_rows(rows.csv()) == rows`) |

## Without `outbound`: an integration takes them

The events no `outbound` delivers can be delivered by a program of your own, anywhere, which
takes them through the [HTTP API](/reference/http-api) and says what came of each. It is the
way out for whatever a connection does not cover yet (a protocol, a conversation of several
calls, a system behind a firewall):

```bash
# up to 50 events, each leased to you for 60 seconds: no one else gets them meanwhile
curl -H "Authorization: Bearer $TOKEN" "https://shop.quadra.run/v1/outbox?events=OrderShipped&limit=50&lease=60"
```

```json
{"events": [{"id": 41, "key": "quadra-3f9a0c1d2b4e5f60-41", "event": "OrderShipped", "tx": 1207,
  "created_at": "2026-11-27T10:14:03Z", "attempts": 0, "data": {"number": "A-1042"}}],
 "lease_until": "2026-11-27T10:15:03Z"}
```

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" -d '{"ids": [41]}' https://shop.quadra.run/v1/outbox/ack
curl -X POST -H "Authorization: Bearer $TOKEN" -d '{"id": 42, "error": "the ERP is down", "retry_after": 300}' https://shop.quadra.run/v1/outbox/nack
```

| Call | Effect |
|---|---|
| `GET /v1/outbox` | pending events whose commit is confirmed, oldest first, that no `outbound` delivers and no one else holds: `events` (a list of kinds), `limit` (1 to 500, 50), `lease` (10 to 3600 seconds, 60). Each is leased to the caller until `lease_until`; one neither acknowledged nor refused by then is handed out again |
| `POST /v1/outbox/ack` | `{"ids": […]}`: delivered. Answers the ids that were pending |
| `POST /v1/outbox/nack` | `{"id", "error", "retry_after"?, "final"?}`: not delivered; handed out again after `retry_after` seconds (30 when not given), or failed for good with `"final": true` |

The same rules as the server's deliveries: at least once (use `key` to ignore what you have
seen), the same states and the same log, with `pull:<user>` as the connection. Only a token
of a user who may edit `User.roles` takes the outbox: its events may carry anything the app
sends out.

## On Quadra Cloud

Each environment of a project (production, a preview, a fork) has values of its own, set
with the CLI:

```bash
quadra cloud env set SHOP_NAME Acme
quadra cloud env set SHOP_TOKEN                     # asks for the value without showing it
quadra cloud env set SHOP_TOKEN --environment feat-x--shop
quadra cloud env list                               # names only, and what quadra.toml declares and is not set
quadra cloud env unset SHOP_NAME
quadra cloud env pull > .env                        # the values themselves: an owner of the team only
```

| Rule | Detail |
|---|---|
| Encrypted | each value is encrypted in the platform's database; `list` never shows one, `pull` (owners only) writes them out for local development |
| Applied | at the app's next start: a running app restarts within seconds |
| Production | a deploy fails while a variable without a default has no value: `✗ a variable is not set: SHOP_TOKEN (declared in quadra.toml, [vars], without a default)` |
| Previews and forks | they copy production's data but never its values, and they deliver no event: their events stay in the outbox, so a copy of production never sends with production's connections |
| Egress | connections reach public addresses only |
| Who | people change and read values; a project's token (CI) cannot |

## In tests

Tests do not read the environment, so that they give the same result everywhere: `env.NAME`
is the variable's `test` value, else its default; reading a variable that has neither fails
the test. Tests send nothing: the events stay in the outbox.

```toml
[vars]
MAX_LINES = { type = "Integer", default = "50", test = "5" }
```

```quadra
test "shipping names the shop":
    let o = Order(number="A1", customer_email="a@example.com")
    o.ship()
    expect o.note == "shipped by Acme"
```
