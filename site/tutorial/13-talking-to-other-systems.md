# Step 13: Talking to other systems

<SandboxLink project="tutorial-13" file="webshop.qd" />

**Goal:** the shop does not live alone. It now has:

- a **web shop** that takes orders online: each one must arrive in Quadra as a confirmed order,
  and the web shop must always show our current products and prices;
- a **carrier**: when an order ships, a parcel is booked through its API, and the tracking
  number it answers is kept on the order;
- a **mail server**, to tell the customer that their order is on its way.

Every message in or out passes the same checks as a clerk's edit. Each one is kept, with a
log of what happened to it. None is lost if the server stops halfway, and none is handled twice.

## Variables and connections

The code never holds an address or a password. `quadra.toml` declares the **variables**, whose
values depend on where the app runs, and the **connections**, the systems we talk to:

```toml
[vars]
WEBSHOP_TOKEN = { secret = true, description = "the API token of the web shop" }
WEBSHOP_HOOK_SECRET = { secret = true, description = "signs the web shop's webhooks" }
CARRIER_KEY = { secret = true, description = "the carrier's API key" }
SMTP_PASSWORD = { secret = true }
SHOP_EMAIL = { default = "shop@beans.example.com", test = "test@beans.example.com" }

# The online shop: it sends us its orders, we send it our products and stock.
[connections.webshop]
kind = "http"
url = "https://beans.example.com/api"
bearer = { var = "WEBSHOP_TOKEN" }
inbound_signature = "hmac"
inbound_secret = { var = "WEBSHOP_HOOK_SECRET" }

# The carrier: we book a parcel, it answers with a tracking number.
[connections.carrier]
kind = "http"
url = "https://api.carrier.example.com/v2"
api_key = { var = "CARRIER_KEY" }
rate = "5/s"

# Our mail server.
[connections.mail]
kind = "smtp"
url = "smtps://smtp.example.com:465"
user = "shop@beans.example.com"
password = { var = "SMTP_PASSWORD" }
from = "Beans & Leaves <shop@beans.example.com>"
```

- A **secret** cannot be written in `quadra.toml`, which is in the repository, and the code
  cannot read one. Only a connection uses it, for its credentials. Its value comes from the
  environment, a `.env` file next to the project, or `quadra cloud env set` on Quadra Cloud.
- A variable that is not secret can be read by actions and reactions as `env.SHOP_EMAIL`.
  Tests use its `test` value.
- `rate = "5/s"` respects the carrier's limit: Quadra never sends faster than that.

## Messages in: `inbound`

The web shop calls a URL of ours with each new order, as JSON (a *webhook*). Its JSON uses its
own names: `name` for the order number, a nested `customer`, `line_items`. A **record
`from json`** describes it once: each field, its type, and where it is in the document.

```quadra
record WebLine from json:
    sku: Code
    quantity: Decimal

record WebOrder from json:
    id: Text
    number: Text at "name"                  # the web shop calls it `name`: "#1001"
    email: Email at "customer.email"        # a path into nested objects
    customer: Text at "customer.name"
    lines: [WebLine] at "line_items"
```

An `inbound` says where messages arrive and what to do with each one:

```quadra
# The web shop POSTs each new order to https://<our app>/in/webshop/orders.
inbound WebOrderIn via webshop path "/orders" as m: WebOrder:
    key m.id                                # sent twice? handled once
    let customer = Customer.where(c => c.email == m.email).first()
        ?? Customer(name=m.customer, email=m.email)
    let order = Order(customer=customer, notes=f"web order {m.number}")
    for l in m.lines:
        let p = Product.where(x => x.sku == l.sku).first() ?? fail f"unknown product {l.sku}"
        OrderLine(order=order, product=p, qty=Qty(l.quantity, p.uom))
    order.confirm()
```

What happens to a message:

1. **Who sent it** is checked first, with the connection's `inbound_signature`. A message
   with a wrong signature is refused and never touches the data.
2. It is **kept raw** before it is handled. The web shop gets its `202` only once the message
   is safe.
3. **Duplicates**: web shops resend a webhook when they are not sure it arrived. A second
   message with the same `key` is recognized and not handled again.
4. It is **read** as a `WebOrder`, then **handled** by the code: one transaction, as the
   system. Everything an order must satisfy still holds. The credit limit, the active
   products and the stock are checked as if a clerk had typed the order. If anything fails
   (`fail`, a rule), nothing is written and the message is `rejected` with the reason, to be
   fixed and retried.

`Qty(l.quantity, p.uom)` turns the web shop's plain number into a quantity in the product's
unit: 2 kg of beans, 4 mugs.

## Messages out: `emit` and `outbound`

Going out takes two steps. A transaction decides **what** happened, with `emit`. An `outbound`
decides **how** it is sent. The event leaves only **after** the transaction commits. An
order cancelled by an error halfway never books a parcel.

`shipping.qd` books a parcel when an order ships:

```quadra
event ParcelRequested(order: Order, number: Text, name: Text, street: Text, city: Text, zip: Text)

on Order enters Shipped as o:
    if let a = o.ship_to:
        emit ParcelRequested(order=o, number=o.number ?? "", name=o.customer.name,
            street=a.street, city=a.city, zip=a.zip)
```

The carrier wants its own JSON, and answers with a tracking number:

```quadra
record CarrierParcel from json:
    reference: Text
    name: Text at "recipient.name"
    street: Text at "recipient.address.street"
    city: Text at "recipient.address.city"
    zip: Text at "recipient.address.postal_code"

record CarrierLabel from json:
    tracking: Text at "tracking_number"


outbound ParcelRequested via carrier as e:
    reply CarrierLabel                      # the answer, read as a CarrierLabel
    retry for 2 days
    key f"parcel-{e.number}"                # booked twice by mistake? the carrier ignores it
    post("/parcels", CarrierParcel(reference=e.number, name=e.name,
        street=e.street, city=e.city, zip=e.zip))
```

The first lines are the **policy**: retry for two days if the carrier is down, and send an
`Idempotency-Key` of our own so that a parcel is never booked twice. The last line is the
**request**. The block cannot write anything: it only builds what is sent.

What came back is handled by a reaction, in a transaction of its own. `tracking` is a new
field of `Order` (in `orders.qd`), editable after the order is frozen:

```quadra
on ParcelRequested delivered as d:
    d.event.order.tracking = d.reply?.tracking

on ParcelRequested failed as f:
    f.event.order.notes = f"the carrier refused the parcel: {f.error}"
```

```quadra{2,6}
    notes: Text?
    tracking: Text?                         # the carrier's tracking number, once it has one
    lines: [OrderLine]
    # …
    frozen when state is not Draft except notes, cancel_reason, tracking
```

### Emails

`Order.ship` has emitted a `ShippingNotice` since step 6, and so far nothing sent it. Now it
goes through the mail server:

```quadra
outbound ShippingNotice via mail as e:
    expires after 2 days                    # an email that late is not worth sending
    let number = e.order.number ?? ""
    email(to=e.to, subject=f"Your order {number} is on its way",
        body=f"Hello {e.order.customer.name},\n\nyour order {number} has left our shop.\n\n{env.SHOP_EMAIL}")
```

### Keeping a catalog in step: `publish`

The web shop must always have our current products. There is no event to write for that.
`publish` says which records go and how, and Quadra sends one each time it changes:

```quadra
record WebProduct from json:
    sku: Code
    name: Text
    price: Decimal
    unit: Text
    available: Decimal


publish Product to webshop as p:
    when p.active
    on name, price, active                  # a change of these fields sends it again
    debounce 30 seconds                     # ten edits in a row: only the last state goes
    put(f"/products/{p.sku}", WebProduct(sku=p.sku, name=p.name,
        price=p.price * Qty(1, p.uom) / 1 EUR, unit=f"{p.uom}",
        available=p.available / Qty(1, p.uom)))
```

Formats carry plain values, not amounts with a currency. `p.price * Qty(1, p.uom) / 1 EUR` is
the price of one unit as a number (`18.00` for the beans).

## Tests play the other side

Tests send nothing and receive nothing:

- `receive` plays a message coming in;
- `expect sent` checks what was emitted;
- `deliver` plays the answer of the other side.

```quadra
const WEB_ORDER = """
{"id": "820982911946154508", "name": "#1001",
 "customer": {"name": "Bruno Neri", "email": "bruno@example.com"},
 "line_items": [{"sku": "BEANS-01", "quantity": 2}, {"sku": "MUG-01", "quantity": 4}]}
"""


test "an order from the web shop is confirmed":
    use shop
    receive WebOrderIn WEB_ORDER
    let o = Order.where(x => x.notes == "web order #1001").first() ?? fail "no order"
    expect o.state is Confirmed
    expect o.customer.name == "Bruno Neri"
    expect o.lines.count() == 2
    expect beans.reserved == 2 kg


test "the same order sent twice is handled once":
    use shop
    receive WebOrderIn WEB_ORDER
    receive WebOrderIn WEB_ORDER
    expect Order.count() == 1


test "an unknown product rejects the whole order":
    use shop
    expect_error "unknown product DECAF-01":
        receive WebOrderIn """
            {"id": "8", "name": "#1003", "customer": {"name": "Bruno", "email": "b@example.com"},
             "line_items": [{"sku": "BEANS-01", "quantity": 1}, {"sku": "DECAF-01", "quantity": 1}]}"""
    expect Order.count() == 0
    expect Customer.count() == 2


test "shipping books a parcel and keeps its tracking number":
    use shop
    let order = Order(customer=anna, lines=[OrderLine(product=mug, qty=2 pcs)])
    order.confirm()
    order.ship()
    expect sent ParcelRequested(order=order, city="Milano", zip="20121")
    deliver ParcelRequested reply CarrierLabel(tracking="1Z999AA10123456784")
    expect order.tracking == "1Z999AA10123456784"


test "a product that changes is sent to the web shop":
    use shop
    mug.price = 8.00 EUR/pcs
    expect sent PublishProductToWebshop(record=mug)
```

The order with an unknown product leaves **nothing** behind, not even the new customer.
The message is one transaction.

## Run it

```bash
quadra test
```

```text
…
46 passed, 0 failed
```

On your machine, put the secrets in a `.env` file next to `quadra.toml` (and keep it out of
git):

```text
WEBSHOP_TOKEN=dev-token
WEBSHOP_HOOK_SECRET=dev-hook-secret
CARRIER_KEY=dev-carrier-key
SMTP_PASSWORD=dev-smtp
```

Start the server (`quadra serve --dev-login`, on port 8080) and play the web shop with
`curl`: sign the body with the shared secret, as the web shop does.

```bash
BODY='{"id": "820982911946154508", "name": "#1001", "customer": {"name": "Bruno Neri", "email": "bruno@example.com"}, "line_items": [{"sku": "BEANS-01", "quantity": 2}, {"sku": "MUG-01", "quantity": 4}]}'
SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac dev-hook-secret | sed 's/.*= //')
curl -s http://127.0.0.1:8080/in/webshop/orders -H "X-Signature: $SIG" \
  -H 'Content-Type: application/json' -d "$BODY"
```

```json
{"id":1}
```

Send it again:

```json
{"duplicate":true,"id":1}
```

And with a wrong signature, as someone pretending to be the web shop:

```json
{"error":"the signature of X-Signature does not match"}
```

Every message is in the **inbox**, with its log:

```bash
quadra inbox list
quadra inbox log 1
```

```text
#1     2026-11-27T18:39:56  WebOrderIn  key 820982911946154508  (done by tx 3)

#1  2026-11-27T18:39:56  receive   received
#1  2026-11-27T18:39:56  handle    done      : transaction 3
#1  2026-11-27T18:39:56  duplicate duplicate : the same message, received again: not handled again
```

Every event going out is in the **outbox**. The products of the demo data are waiting for
the web shop, which does not exist on your machine:

```bash
quadra outbox list
quadra outbox log 1
```

```text
#1     tx 2      2026-11-27T18:39:45  PublishProductToWebshop  {"record":{"ent":"Product","ref":30}}  (pending)
#2     tx 2      2026-11-27T18:39:45  PublishProductToWebshop  {"record":{"ent":"Product","ref":31}}  (pending)
#3     tx 2      2026-11-27T18:39:45  PublishProductToWebshop  {"record":{"ent":"Product","ref":32}}  (pending)

#1  2026-11-27T18:40:15  attempt  pending     via webshop  24 ms  beans.example.com: failed to lookup address information: nodename nor servname provided, or not known
```

They are tried again, waiting longer each time, for a day. `quadra outbox retry`, `skip` and
`inbox replay --dry-run` are there for the day something goes wrong
([CLI](/reference/cli)). The studio shows the same thing under **Integrations**.

## Try it

1. Add `endpoint GET "/stock" via webshop as q: StockQuery -> StockAnswer:` so that the web
   shop can ask the stock of a product at checkout, and answer `p.available`
   ([endpoint](/reference/connections#endpoint-answered-at-once)).
2. Make the web shop's orders wait for stock instead of being rejected: `retry for 1 hour`
   in the inbound, and a test where the restock arrives after the order.
3. Send the `ShippingNotice` only between 8:00 and 20:00 (`window "08:00" .. "20:00"`).

## Recap

- `quadra.toml` declares **variables** (secrets never in the code) and **connections**
  (`http`, `smtp`…).
- A **record `from json`** describes another system's document: names, paths, types.
- `inbound` receives messages: the sender is checked, the message is kept, duplicates are
  dropped by `key`, and the code runs in a transaction with every rule of the model.
- `emit` decides what happened, `outbound` how it goes out, after the commit: `reply`,
  `retry for`, `key`, and `on … delivered` / `failed` for the outcome.
- `publish` keeps another system in step with a kind of record.
- Tests `receive`, `expect sent` and `deliver`. Production keeps every message in the inbox
  and outbox, with a log.

[Next: files, formats and databases →](./14-files-formats-and-databases)
