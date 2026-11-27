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
{"id": 12, "event": "ShipmentReady", "tx": 88, "created_at": "2026-11-27T10:14:03", "data": {"order": "A-1042", "carrier": "DHL", "weight_kg": "2.5", "label": "A-1042/DHL"}}
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

### Webhooks

Through an HTTP connection an event is a `POST` of JSON to the connection's `url` followed by
the `path`:

```json
{"id": 41, "event": "OrderShipped", "tx": 1207, "created_at": "2026-11-27T10:14:03", "data": {"number": "A-1042"}}
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

### Attempts and failures

| Answer | What happens |
|---|---|
| HTTP 2xx, SMTP accepted | delivered |
| HTTP 408, 429 or 5xx, a timeout, no connection, SMTP 4xx | tried again: after 30 s, then twice as long each time up to an hour, for a day |
| any other HTTP answer (a 4xx, a redirect: redirects are not followed), SMTP 5xx | failed for good |
| a day of failed attempts | failed for good |

The outbox keeps each event's attempts, the time of the next one and the last error;
`quadra outbox list` shows them and `quadra outbox retry <id…>` makes failed events pending
again ([CLI](/reference/cli#events-quadra-outbox)). Delivery is **at least once**: if the
server stops between sending and recording the answer, the event is sent again with the same
`Idempotency-Key`, which the receiver should use to ignore what it has already seen.

On Quadra Cloud connections only reach public addresses: a name that resolves to a private,
loopback or link-local address (the clouds' metadata, `169.254.169.254`) is refused, and so
is a redirect. Previews and forks deliver nothing ([On Quadra Cloud](#on-quadra-cloud)).

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
