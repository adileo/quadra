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
| The request | the block ends with it. HTTP: `post(path, body)`, `put(path, body)`, `patch(path, body)`, `delete(path)`; the path is appended to the connection's url (and to the outbound's `path`), the body is a record `from json` (sent as `application/json`) or text. SMTP: `email(to=…, subject=…, body=…, html=…)` (with `html`, the email has both versions) |
| `reply T` | the first line of the block: the answer of a 2xx, read as a record `from json`, is `d.reply` in `on … delivered` (none when it does not fit, said in the log) |
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

## Formats: `record … from json`

The documents of another system (a shop's order, a carrier's label, the answer of an API) are
described as records `from json`: what a field is called there, where it is, what type it has.
Reading and writing them are pure functions, the same text always giving the same record:

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
| Not in a document | a record of the database (`Customer`): send its id, or a record `from json` made of its fields; amounts with a currency: a `Decimal` and a `Code` |
| Only records | `entity X from json` is refused: what comes from outside has no identity until the code makes a record of it |

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
