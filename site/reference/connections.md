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
`api_key`, `password`, `client_secret`, `signing_secret`, `client_certificate`) must come from
a **secret** variable:

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
| `oauth2_token_url`, `client_id`, `client_secret`, `scope`, `client_auth` | OAuth2 client credentials (below) |
| `oauth2_grant = "authorization_code"`, `oauth2_authorize_url` | OAuth2 with the consent of a user instead ([below](#oauth2-with-the-users-consent)) |
| `signing_secret` | signs every request (see [Webhooks](#webhooks)); `signature_…` say how (below) |
| `client_certificate` | the client's certificate and its private key, PEM, for servers that ask for one (mutual TLS) |
| `server_ca` | the certificate (PEM) of the authority that signed the server's, when it is a private one (an ERP of the company's network): then only it is trusted |
| `timeout` | from `"1s"` to `"120s"`; `"10s"` when not given |
| `insecure` | `true` allows `http://`: for a test receiver on your machine |

One authentication at most: `bearer`, `api_key`, `user` and `password`, or OAuth2. A
certificate and a signature go with any of them.

**OAuth2.** With `oauth2_token_url`, Quadra asks the token itself (the *client credentials*
grant: `grant_type=client_credentials`, with `scope` when given), keeps it until a minute
before it expires (`expires_in`, an hour when not said), and sends it as
`Authorization: Bearer <token>`. The client's id and secret go as basic authentication, or in
the form with `client_auth = "body"`. A request refused with 401 gets a new token and is sent
again once, in the same attempt; refused again, it is a failed attempt like any other.

```toml
[connections.erp]
kind = "http"
url = "https://erp.example.com/api"
oauth2_token_url = "https://login.example.com/oauth2/token"
client_id = { var = "ERP_CLIENT_ID" }
client_secret = { var = "ERP_CLIENT_SECRET" }
scope = "orders.write"
client_certificate = { var = "ERP_CLIENT_PEM" }    # if the ERP asks for one
```

**A signature in the receiver's format.** Without `signature_header`, `signing_secret` writes
Quadra's own `Quadra-Signature` ([Webhooks](#webhooks)). A receiver that checks a signature of
its own says how it wants it:

| Key | Meaning |
|---|---|
| `signature_header` | the header with the HMAC-SHA256 (`X-Signature`) |
| `signature_encoding` | `"hex"` (when not given) or `"base64"` |
| `signature_prefix` | text before it (`"sha256="`) |
| `signature_timestamp_header` | a header with the unix seconds, signed with the body as `"<seconds>.<body>"` (without it the body alone is signed) |

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

### `kind = "imap"`: mailboxes and PEC

A mailbox read over IMAP: an ordinary one, or a PEC mailbox (*posta elettronica
certificata*), whose certified messages and receipts arrive as mails. Each mail is a message
of an [`inbound`](#inbound-messages-in) that pulls it:

```toml
[vars]
PEC_PASSWORD = { secret = true }

[connections.pec]
kind = "imap"
url = "imaps://imaps.pec.aruba.it"
user = "amministrazione@pec.acme.it"
password = { var = "PEC_PASSWORD" }
```

| Key | Meaning |
|---|---|
| `url` | `imaps://host` (TLS, port 993 when not given); plain `imap://host:143` only with `insecure = true` |
| `user` | the login of the mailbox |
| `password` | its password, a secret variable |
| `oauth2_grant = "authorization_code"`, `oauth2_authorize_url`, `oauth2_token_url`, `client_id`, `client_secret`, `scope` | instead of a password, OAuth2 with the user's consent (`AUTHENTICATE XOAUTH2`): Gmail, Microsoft 365 ([below](#oauth2-with-the-users-consent)) |
| `timeout` | from `"1s"` to `"600s"`; `"30s"` when not given |
| `insecure` | `true` allows `imap://` without TLS: for a local test server such as GreenMail |
| `location` | `"agent:<name>"`: read by an on-premise agent, in the network of the mail server (with a password only) |

```quadra
entity Invoice:
    number: Text
    supplier: Text
    total: Decimal
    pec_id: Text
    file_name: Text

entity SentPec:
    message_id: Text
    subject: Text
    accepted_at: DateTime?
    delivered_at: DateTime?

entity PecReceipt:
    kind: Text
    pec_message_id: Text
    at: DateTime
    sent: SentPec?

record MailAddress from json:
    name: Text?
    address: Text?

record MailAttachment from json:
    name: Text
    content_type: Text
    size: Int
    sha256: Text
    content: Text?
    text: Text?

record OriginalMail from json:
    message_id: Text?
    subject: Text?
    sender: MailAddress? at "from"
    attachments: [MailAttachment]

record PecRecipient from json:
    address: Text
    kind: Text? at "type"

record PecData from json:
    kind: Text at "type"
    message_id: Text?
    id: Text?
    sender: Text?
    date: DateTime?
    recipients: [PecRecipient]
    original: OriginalMail?

record PecMail from json:
    message_id: Text?
    subject: Text?
    sender: MailAddress? at "from"
    date: DateTime?
    text: Text?
    attachments: [MailAttachment]
    pec: PecData?

record FatturaPA from xml(root="p:FatturaElettronica"):
    ns p "http://ivaservizi.agenziaentrate.gov.it/docs/xsd/fatture/v1.2"
    number: Text at "FatturaElettronicaBody/DatiGenerali/DatiGeneraliDocumento/Numero"
    total: Decimal at "FatturaElettronicaBody/DatiGenerali/DatiGeneraliDocumento/ImportoTotaleDocumento"
    supplier: Text at "FatturaElettronicaHeader/CedentePrestatore/DatiAnagrafici/Anagrafica/Denominazione"

inbound PecIn via pec as m: PecMail:
    pull "INBOX" every 2 minutes
    key m.message_id
    let p = m.pec ?? fail "not a PEC"
    if p.kind == "posta-certificata":
        let o = p.original ?? fail "a PEC without its message"
        for a in o.attachments:
            if a.name.ends_with(".xml"):
                let f = FatturaPA.parse(a.text ?? fail "an invoice that is not text")
                let i = Invoice(number=f.number, supplier=f.supplier, total=f.total, pec_id=p.id ?? "", file_name=a.name)
    else:
        let id = p.message_id ?? fail "a receipt without the id of its message"
        let at = p.date ?? fail "a receipt without a date"
        let s = SentPec.where(x => x.message_id == id).first()
        let r = PecReceipt(kind=p.kind, pec_message_id=id, at=at, sent=s)
        if let sent = s:
            if p.kind == "accettazione":
                sent.accepted_at = at
            if p.kind == "avvenuta-consegna":
                sent.delivered_at = at
```

`pull "INBOX"` names the mailbox (`"INBOX"`, `"Fatture/2026"`), `every` how often it is read
(a minute at least). Each mail is kept, read as the inbound's record and handled like any
message: the same states, duplicates, log and commands. Use `key m.message_id`: the mail's
own id is what tells it apart.

**The document of a mail.** An inbound reads each mail as this JSON document, with a record
`from json` that declares what it uses (the rest is ignored):

```json
{
  "message_id": "20261127.4711@shop.example.com",
  "from": {"name": "Shop Orders", "address": "orders@shop.example.com"},
  "to": [{"name": null, "address": "acme@example.com"}],
  "cc": [],
  "reply_to": [],
  "subject": "Order 1042",
  "date": "2026-11-27T09:14:03Z",
  "text": "Thank you for your order 1042.",
  "html": "<p><b>Thank you</b> for your order 1042.</p>",
  "attachments": [
    {"name": "order-1042.csv", "content_type": "text/csv", "size": 14,
     "sha256": "6f1c…", "content": "c2t1O3F0eQpBLTE7Mgo=", "text": "sku;qty\nA-1;2\n",
     "truncated": false}
  ],
  "pec": null
}
```

| Key | Value |
|---|---|
| `message_id` | the `Message-ID` header, without `<` `>` (`null` when the mail has none) |
| `from`, `to`, `cc`, `reply_to` | `{"name", "address"}`: `from` one, the others lists |
| `subject` | the subject, decoded |
| `date` | the `Date` header as an instant in UTC, `2026-11-27T09:14:03Z` |
| `text`, `html` | the first plain-text and HTML bodies, decoded to UTF-8 (`null` when there is none) |
| `attachments` | every other part, in order (below) |
| `pec` | for a PEC, what its `daticert.xml` certifies (below); `null` for any other mail |
| `truncated`, `size` | only on a mail larger than 32 MiB: taken with its headers alone, no bodies or attachments, `size` its size in bytes |

**Attachments.**

| Key | Value |
|---|---|
| `name` | its file name (`attachment-2`, `message-3.eml` when it has none) |
| `content_type` | `application/pdf`, `text/csv`, `message/rfc822` (an attached mail)… |
| `size`, `sha256` | its size in bytes and the hex SHA-256 of its content, decoded |
| `content` | the content, base64 |
| `text` | the content as text, when it is text (`text/*`, XML, JSON, `.csv`, `.txt`, `.eml`) in UTF-8: what `Invoice.parse(a.text)` reads; `null` otherwise |
| `truncated` | `true` when the content is left out: an attachment larger than 16 MiB, or past 24 MiB of attachments in one mail; `size` and `sha256` are still given |

**PEC.** A PEC is a transport envelope (`X-Trasporto`) or a receipt (`X-Ricevuta`) with a
`daticert.xml`, signed by the provider (`smime.p7s`). Its `pec` says what the provider
certifies; the envelope's own parts (`daticert.xml`, `postacert.eml`, `smime.p7s`) are read
into it and not listed among the attachments.

```json
"pec": {
  "type": "posta-certificata",
  "error": "nessuno",
  "error_detail": null,
  "id": "opec21031.20261127101403.04711.06.1.1@pec.example.it",
  "message_id": "FAT2026118.20261127@mail.fornitore.example.it",
  "sender": "fatture@pec.fornitore.example.it",
  "recipients": [{"address": "amministrazione@pec.acme.it", "type": "certificato"}],
  "reply_to": "fatture@pec.fornitore.example.it",
  "subject": "Fattura 2026/118",
  "provider": "Example PEC S.p.A.",
  "date": "2026-11-27T09:14:03Z",
  "receipt": null,
  "delivered_to": null,
  "original": {"message_id": "FAT2026118.20261127@mail.fornitore.example.it", "subject": "Fattura 2026/118", "from": {…}, "attachments": [{"name": "IT01234567890_00118.xml", …}], …}
}
```

| Key | Value (from `daticert.xml`) |
|---|---|
| `type` | `posta-certificata` (a certified message), `accettazione`, `non-accettazione`, `presa-in-carico`, `avvenuta-consegna`, `errore-consegna`, `preavviso-errore-consegna`, `rilevazione-virus` |
| `error`, `error_detail` | `nessuno`, `no-dest`, `no-dominio`, `virus`, `altro`; and the provider's explanation (`errore-esteso`) |
| `id` | the provider's identifier of the message (`identificativo`): the same on the message and on all its receipts |
| `message_id` | the `Message-ID` of the message certified, without `<` `>` (`msgid`): on a receipt, the id of the PEC it is about |
| `sender`, `recipients`, `reply_to`, `subject` | as certified; each recipient with its `type`, `certificato` or `esterno` |
| `provider` | the provider that certifies it (`gestore-emittente`) |
| `date` | when, as an instant in UTC (the provider writes the day, the time and the zone) |
| `receipt`, `delivered_to` | on an `avvenuta-consegna`: `completa`, `breve` or `sintetica`, and the mailbox it was delivered to |
| `original` | the message carried (`postacert.eml`: a `posta-certificata`, a complete receipt), as the same document without `pec` |

A receipt links to the PEC it is about by `pec.message_id`: the `Message-ID` the app gave the
PEC it sent (keep it on the record that sent it, as `SentPec` above). The signature
`smime.p7s` is not verified: the mailbox is the provider's, read over TLS, and the provider is
who signs; keep the mailbox's `Quadra/processed` folder (or the provider's archive) where the
law asks for the messages as received.

**Where the mails go.** A mail is never deleted, and taking it does not mark it read
(`BODY.PEEK`). Once handled for good, it is marked read and moved at the next pull:

| State of the message | Moved to |
|---|---|
| `done`, `skipped`, `superseded` | `Quadra/processed` |
| `rejected` | `Quadra/rejected` (the error stays in its log, `quadra inbox log`) |

The folders are created when missing, with the server's separator (`Quadra.processed` where
it is `.`). The move is `UID MOVE` where the server has it, else a copy, the original flagged
`\Deleted` and expunged (`UID EXPUNGE` with UIDPLUS; a server without it expunges every mail
flagged `\Deleted` in that mailbox). A mail someone moved meanwhile counts as settled.

| Rule | Detail |
|---|---|
| Taken once | each pull takes the mails after its cursor, the mailbox's `UIDVALIDITY` and the last UID taken, kept with the mails in one write: a restart neither loses nor takes again what it took |
| The first pull | takes every mail already in the mailbox, oldest first: point the inbound at a mailbox or folder of its own |
| Per pull | at most 100 mails and about 64 MiB; the rest at the next one |
| Renumbered | a server that changes `UIDVALIDITY` hands out its mails again: with `key m.message_id` they are duplicates, and settling finds them by their `Message-ID` |
| Errors | no answer, a dropped connection: the pull is tried again (within five minutes); a login refused: in the log of message 0, until the password is fixed |
| Not done | sending through PEC (use an `smtp` connection to the PEC provider), IDLE (mails come at the next pull), S/MIME and `.p7m` signatures (read as attachments, not verified or opened) |

### OAuth2 with the user's consent

Some systems are reached only with the consent of one of their users: an app of a shop
(Shopify), a mailbox of Gmail or Microsoft 365. The connection says where to ask
(`oauth2_grant = "authorization_code"`); an admin of the app gives the consent once, in the
provider's page, and Quadra keeps the tokens and refreshes them.

```toml
[vars]
SHOPIFY_CLIENT_ID = {}
SHOPIFY_CLIENT_SECRET = { secret = true }
GOOGLE_CLIENT_SECRET = { secret = true }

[connections.shop]
kind = "http"
url = "https://acme.myshopify.com/admin/api/2026-10"
oauth2_grant = "authorization_code"
oauth2_authorize_url = "https://acme.myshopify.com/admin/oauth/authorize"
oauth2_token_url = "https://acme.myshopify.com/admin/oauth/access_token"
client_id = { var = "SHOPIFY_CLIENT_ID" }
client_secret = { var = "SHOPIFY_CLIENT_SECRET" }
client_auth = "body"
scope = "read_orders,write_fulfillments"

[connections.inbox]
kind = "imap"
url = "imaps://imap.gmail.com"
user = "ordini@acme.it"
oauth2_grant = "authorization_code"
oauth2_authorize_url = "https://accounts.google.com/o/oauth2/v2/auth?access_type=offline&prompt=consent"
oauth2_token_url = "https://oauth2.googleapis.com/token"
client_id = "123456789.apps.googleusercontent.com"
client_secret = { var = "GOOGLE_CLIENT_SECRET" }
scope = "https://mail.google.com/"
```

| Key | Meaning |
|---|---|
| `oauth2_grant` | `"authorization_code"`: the user's consent. `"client_credentials"` (the default, HTTP only) is the [client's own token](#kind-http) |
| `oauth2_authorize_url` | the provider's page that asks the user; parameters of the provider's own may be written in it (`access_type=offline&prompt=consent`, which Google needs to give a refresh token) |
| `oauth2_token_url` | where the code is exchanged and the tokens refreshed |
| `client_id`, `client_secret` | the app registered with the provider; the secret a secret variable |
| `client_auth` | `"basic"` (the default) or `"body"`: how the client's id and secret go to the token url |
| `scope` | what the consent covers, in the provider's words: `https://mail.google.com/` for Gmail, `https://outlook.office.com/IMAP.AccessAsUser.All offline_access` for Microsoft 365 (`offline_access` gives the refresh token) |

Register `https://<app>/oauth/<connection>/callback` with the provider as the redirect URL
(`https://shop.quadra.run/oauth/shop/callback`).

| Step | What happens |
|---|---|
| Consent | an admin (who may edit `User.roles`) opens `https://<app>/v1/connections/<name>/authorize`: a redirect to the provider's page, with a `state` and a PKCE challenge (S256). The state is sealed by the server and holds ten minutes: it carries the connection, the PKCE verifier and who asked, so no one else can complete it |
| Callback | the provider sends the admin back to `/oauth/<name>/callback?code=…&state=…`; the server checks the state, exchanges the code with the verifier, keeps the tokens and shows `` `shop` is connected `` |
| Use | requests (`outbound`, `pull`) send `Authorization: Bearer <access token>`; a mailbox logs in with `AUTHENTICATE XOAUTH2` |
| Refresh | a minute before the access token expires (`expires_in`; one without it does not expire), and once when the other side answers 401 (or refuses the mailbox's login), the refresh token gets a new one, kept at once. A provider that rotates refresh tokens gets one refresh at a time |
| Revoked | a refresh refused (`invalid_grant`): events and pulls wait, without a time limit, with ``the provider refused to refresh the token of `shop` (…): the consent was revoked or expired; an admin connects it again at /v1/connections/shop/authorize``; connecting again resumes them |
| Not connected | the same wait, with `` `shop` is not connected: an admin gives the consent at /v1/connections/shop/authorize `` |
| Disconnect | `POST /v1/connections/<name>/disconnect` forgets the tokens (revoke the app at the provider too) |

**Where the tokens are.** In the app's database (`_quadra_oauth`, one row per connection),
encrypted with AES-256-GCM under the key `QUADRA_TOKEN_KEY` of the server's environment, bound
to the connection's name. They go with the database (backups, its log) and never in the clear;
they never appear in the logs, the API or the studio. Without the key a connection with consent
waits, saying so, and nothing is kept:

```bash
export QUADRA_TOKEN_KEY=$(openssl rand -base64 32)    # 32 bytes, base64 or hex; quadra serve reads it
```

| Where | The key |
|---|---|
| `quadra serve`, `quadra dev` | the process's environment (keep it with the server's other secrets: a lost key means connecting again) |
| Quadra Cloud | the platform's servers have it: nothing to set |

**What is shown.** `GET /v1/integrations` (the studio's page "Integrations") and
`GET /v1/connections` list each connection, and for one with consent:

```json
{"name": "shop", "kind": "http", "location": null,
 "oauth2": {"grant": "authorization_code", "connected": true, "scope": "read_orders,write_fulfillments",
            "expires_at": "2026-11-27T11:14:03Z", "connected_at": "2026-11-27T10:14:03Z",
            "connected_by": "user:1", "refreshed_at": null, "error": null,
            "authorize": "/v1/connections/shop/authorize"}}
```

A connection with consent runs on the server only: `location = "agent:<name>"` is refused
(`OAuth2 with the user's consent runs on the server only: its tokens are not handed to an
on-premise agent`). Tests never ask for tokens: they send and receive nothing.

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

### Who owns a field

When both systems change a record (the shop and Quadra each edit products), say which side
owns each field, and let each side send only what it owns: `publish … on` lists the fields
Quadra owns, and the `inbound` writes only the fields the other system owns. A change of the
shop's fields that comes in is then never sent back, and the shop never overwrites Quadra's:

```quadra
publish Product to shop as p:
    on price, name                      # Quadra owns price and name
    put(f"/products/{p.sku}", ShopProduct(sku=p.sku, price=p.price, name=p.name))

inbound StockIn via shop path "/inventory" as m: ShopStock:
    latest by m.sku using m.updated_at
    let p = Product.where(x => x.sku == m.sku).first() ?? fail "unknown sku"
    p.stock = m.available               # the shop owns the stock
```

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

An event that is not sent has reactions too: `on OrderPlaced expired as x:` (its `expires
after` passed) and `on OrderPlaced skipped as x:` (its `unless` held), with `x.event`, `x.at`,
`x.attempts` and `x.reason` (`expired: not sent within 1 day of its emit`, `unless
e.order.cancelled`), in a transaction with the cause `outbound:OrderPlaced#41 expired`.

A message rejected for good, as it came (not its record) or by its code (once `retry for` has
passed), runs `on OrderIn rejected as r:`: the place to tell someone, or to open a task for the
person who fixes it.

```quadra
on OrderIn rejected as r:
    let t = Task(title=f"Shop order not taken: {r.error}", detail=r.message, team=sales)
```

| In `r` | Value |
|---|---|
| `r.message` | the body, as it came |
| `r.key` | its key (else its hash) |
| `r.at` | when it came |
| `r.attempts` | the times its code ran |
| `r.error` | why it was rejected |

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

The log of every step, of the events going out and of the messages coming in, is kept 90
days, then pruned; the events and messages themselves stay, with their final state, error and
attempts. Another period:

```toml
[integrations]
log_days = 365
```

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

### The policy of an inbound

Lines after `key`, before the code, say how its messages are handled when they do not come
as the code expects:

```quadra
inbound StockIn via shop path "/inventory" as m: ShopStock:
    latest by m.sku using m.updated_at       # an older update arriving late is dropped
    retry for 1 hour                         # a product not here yet: tried again meanwhile
    backoff 30 seconds .. 5 minutes
    let p = Product.where(x => x.sku == m.sku).first() ?? fail "unknown sku"
    p.stock = m.available

inbound MovesIn via pos path "/moves" as m: PosMove:
    key m.id
    ordered by m.register                    # each register's moves one at a time, in order
    let r = Register.where(x => x.code == m.register).first() ?? fail "unknown register"
    r.record(m.amount)
```

| Line | Effect |
|---|---|
| `retry for D` | a message whose code fails is tried again, until `D` after it came; then `rejected` (`… (tried for 1 hour)`). It stays `received` meanwhile, each try in its log |
| `backoff A .. B` | the waits between those tries: `A`, doubled each time up to `B` (30 seconds .. 1 hour when not said) |
| `ordered by K` | one at a time per value of `K`, in the order received: a message waiting to be tried again, or rejected, holds back the next ones of its value (not the others) until it is done, retried or skipped |
| `latest by K using T` | a message whose `T` is not newer than that of one of the same `K` already handled is `superseded`, not handled: a stock, a price, a status sent out of order. `T` is an instant, a date, a number or a text that sorts |

Without `ordered by`, the messages are handled in the order received, and one that fails does
not hold back the others. `ordered by` and `latest by` are read when the message comes, from
its record; an inbound has one or the other.

The messages, their states (`received`, `done`, `rejected`, `skipped`, `superseded`) and the
log of each, with the same commands as the outbox:

```bash
quadra inbox list --state rejected
quadra inbox log 12
quadra inbox retry 12                     # handled again by the code deployed now
quadra inbox replay 12 --dry-run          # what the code of now would do with it, nothing written
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

## Files: `kind = "sftp"` and `kind = "ftp"`

A cash register that drops its closing in a folder, a bank that exchanges files, an AS/400
that exports fixed-width records: their files are pulled from an SFTP or FTP server, each a
message of an `inbound`, and an `outbound` writes files there.

```toml
[vars]
POS_SFTP_PASSWORD = { secret = true }
BANK_FTP_PASSWORD = { secret = true }

[connections.pos]
kind = "sftp"
url = "sftp://files.example.com"            # sftp://host:2222/data: another port, a base folder
user = "quadra"
password = { var = "POS_SFTP_PASSWORD" }    # or private_key = { var = "POS_SFTP_KEY" }
known_host = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl"

[connections.bank]
kind = "ftp"
url = "ftps://ftp.bank.example.com"
user = "acme"
password = { var = "BANK_FTP_PASSWORD" }
```

| Key | Meaning |
|---|---|
| `url` | `sftp://host[:port][/folder]`, `ftps://host[:port][/folder]` (FTP with explicit TLS, `AUTH TLS`); plain `ftp://` only with `insecure = true`. The folder, when given, is where the paths start; else the folder the user logs in to |
| `user`, `password` | the login; `password` a secret variable |
| `private_key` | SFTP: a secret variable with the private key (OpenSSH or PEM), instead of the password |
| `known_host` | SFTP, required: the server's public key as a line of `known_hosts` (`ssh-keyscan -t ed25519 host`). A server that shows another key is refused, as someone posing as it: `files.example.com:22 showed the key ssh-ed25519 SHA256:…, not the one of known_host: refused` |
| `timeout` | of each command, from `"1s"` to `"600s"` (`"30s"` when not given); reading or writing a whole file has ten times as long |
| `insecure` | `true` allows `ftp://` without TLS: a test server, or one reached through an agent in its own network |
| `location` | `"agent:<name>"`: the files are reached by the on-premise agent of that name |

FTP goes in passive mode, binary, and its data connections go to the address of the server
it logged in to. A refused login or key is in the log of the pull and does not get better by
trying again; a server down or slow is tried again at the next pull.

### Pulled: a folder, then `processed/` and `rejected/`

```quadra
entity Sale:
    store: Code
    sku: Code
    qty: Int
    amount: Decimal

record PosRow from csv(sep=";", decimal=","):
    store: Code at "NEGOZIO"
    sku: Code at "SKU"
    qty: Int at "QTA"
    amount: Decimal at "IMPORTO" decimals 2

inbound Closing via pos as r: PosRow:
    pull "in/chiusure/*.csv" every 5 minutes
    atomic per file
    if r.qty == 0:
        fail "a quantity of zero"
    let s = Sale(store=r.store, sku=r.sku, qty=r.qty, amount=r.amount)
```

| Step | Rule |
|---|---|
| Which files | `pull "in/chiusure/*.csv"`: the folder `in/chiusure` and, in it, the files whose name matches the last part (`*` any characters, `?` one). Not the folders, not the names that start with `.`. Oldest first by their time of change, up to 100 per pull |
| Still being written | a file changed in the last minute is looked at twice, two seconds apart, and taken only if its size and time did not change; else at the next pull. A sender that can should upload under a name starting with `.` (or another extension) and rename it when done |
| A message | each file is a message of the inbound, read in the encoding of its record (a file that is not UTF-8 is kept as its bytes); its key is its path and the hash of its content, so a file listed again before it is moved is a duplicate, never handled twice |
| Handled | a file of rows (`from csv`, `from fixed`) runs the code once per row, as `atomic` says (below); a document (`from json`, `from xml`) once |
| Moved | once handled, at the next pull (brought forward to a few seconds after the last file waiting is handled): to `processed/` beside it (done, the rows refused reported) or `rejected/` (rejected); the folders are made when missing |
| The report | when rows were refused or the file was rejected, `<file>.report.txt` is written beside it there, before the file is moved: a line that says what happened, then a line per row (up to 1,000) with its line and why |
| Again | a file of the same name and content put in the folder again is not handled again: it is moved, beside the first (`a.csv.20261127-101403`). Another content under the same name is a new message |
| Once | the server may stop at any point: a file kept but not handled is handled after the start, one handled is not handled again, one handled but not moved is moved at the next pull |

```text
in/chiusure/processed/2026-11-27.csv
in/chiusure/processed/2026-11-27.csv.report.txt

PosRow: 2 rows of 1200 refused, the others are in
line 43, column `QTA`: an integer was expected, not `many`
line 701: a quantity of zero
```

### `atomic per file`, `atomic per row`

How the rows of a file are committed, a line of an inbound whose record is `from csv` or
`from fixed`:

| | `atomic per file` (when not said) | `atomic per row` |
|---|---|---|
| A row that cannot be read | the file is `rejected` and nothing is written; the report lists every such line, with its column | refused, in the report; the others go on |
| A row whose code fails | the file is `rejected` and nothing is written; the report says that row | refused, in the report; the others go on |
| Committed | all the rows in one transaction, `inbound:Closing#12 handled` | by chunks of 500 rows, each a transaction (`inbound:Closing#12/lines 2-501 handled`); a chunk where a row fails is committed without it, one that fails at its commit (a rule over its rows) row by row |
| The file | `processed/`, or `rejected/` with its report | `processed/` with the report of the rows refused, or `rejected/` when every row was refused |
| Stopped halfway | nothing was written: handled again | the chunks committed are not run again |

Per file is right when the file is one thing (a closing, a bank statement: half of it is
wrong). Per row when each row stands alone (a price list, a stock count), and a few bad rows
should not hold back the others. A file of 100,000 rows comes in, either way, in a few
seconds.

### `write`: files out

```quadra
event SaleExported(store: Code, sku: Code, qty: Int, amount: Decimal)

record HostRow from fixed(encoding="ibm-1144"):
    store: Code at "1..4"
    sku: Text at "5..12"
    qty: Int at "13..15" zoned
    amount: Decimal at "16..19" packed decimals 2

outbound SaleExported via pos as e:
    write(f"out/{e.store}/{e.sku}.dat", [HostRow(store=e.store, sku=e.sku, qty=e.qty, amount=e.amount)])
```

`write(path, content)` ends the block of an outbound to a file connection. `path` is in the
connection's folder (no `..`, not from `/`); its folders are made when missing. The content
is text (written as UTF-8), a record `from json` or `from xml` (its document), or a list of
rows of a record `from csv` or `from fixed`, written in the record's encoding: latin1, or
EBCDIC with its packed numbers, byte for byte. The file is written under a hidden name in
the same folder (`.a.dat.quadra-….part`) and renamed into place once complete: whoever
reads the folder never sees half a file. A file already there is replaced.

## Databases of other systems

An ERP, a warehouse system, an AS/400 that publishes nothing over HTTP can still be read and
written where its data is: its database. Quadra reads a table a piece at a time, from where it
stopped, and writes rows into tables agreed for the exchange; it never runs other SQL there.

```toml
[vars]
ERP_DB_PASSWORD = { secret = true }
WMS_DB_PASSWORD = { secret = true }
AS400_PASSWORD = { secret = true }

[connections.erp_db]
kind = "postgres"
url = "postgres://erp-db.example.com:5432/erp"
user = "quadra"
password = { var = "ERP_DB_PASSWORD" }

[connections.wms_db]
kind = "mysql"                                   # or "sqlserver": sqlserver://host:1433/database
url = "mysql://wms.example.com:3306/wms"
user = "quadra"
password = { var = "WMS_DB_PASSWORD" }
timeout = "60s"

[connections.as400]
kind = "odbc"
url = "DSN=AS400;CommitMode=0"                  # the DSN of the agent's machine, or a whole connection string
user = "QUADRA"
password = { var = "AS400_PASSWORD" }
location = "agent:milano"                       # ODBC runs only in an agent
insecure = true                                 # the agent is in the same network
```

| Key | Meaning |
|---|---|
| `kind` | `postgres`, `mysql`, `sqlserver`, `odbc` |
| `url` | `postgres://host:5432/database`, `mysql://host:3306/database`, `sqlserver://host:1433/database`, without credentials; for `odbc` the DSN (`DSN=AS400`) or a connection string (`Driver={IBM i Access ODBC Driver};System=as400.local`) |
| `user`, `password` | the login; the password from a secret variable. For `odbc` they are added to the connection string as `UID` and `PWD` |
| `timeout` | of connecting and of each query, from `"1s"` to `"600s"`; `"30s"` when not given |
| `insecure` | `true`: no TLS, for a database in the same network as the agent that reaches it. Without it the connection is TLS, the server's certificate checked against the authorities the system trusts |
| `location` | `"agent:<name>"`: reached by that on-premise agent, in the database's own network, which holds the credentials |

Give Quadra a user of its own on the database, allowed to read the tables it pulls and to
insert into the exchange tables, and nothing else.

### Reading a table: `pull "TABLE"`

```quadra
record ErpOrder from json:
    id: Int at "ORDER_ID"
    number: Text at "ORDER_NO"
    customer: Text at "CUSTOMER_CODE"
    total: Decimal at "TOTAL"
    changed: DateTime at "UPDATED_AT"

inbound OrderIn via erp_db as m: ErpOrder:
    pull "erp.orders" every 5 minutes
    cursor m.changed
    let o = Order(number=m.number, total=m.total)
```

| Rule | Detail |
|---|---|
| `pull "TABLE"` | a table or a view, with its schema (`erp.orders`, `ERPLIB.ORDERS`): letters, digits, `_` and `$`. The name is quoted as written, so it is case-sensitive: on PostgreSQL a table created without quotes has a lower-case name (`orders`, not `ORDERS`) |
| `cursor m.field` | needed: the column the rows are read in the order of (the field's `at`, else its name). Each pull reads `SELECT * FROM table WHERE column >= <cursor> ORDER BY column`, 1000 rows at most, the cursor bound as a parameter and compared in the column's type; the first pull reads the rows whose column is not null |
| The cursor | after a pull, the value of the last row's column, kept with the messages in one write: a server that stops starts again from there, never from the start, and loses nothing |
| `>=` | the rows of the last value are read again and are duplicates, dropped quietly; a row written later with that same value is not lost, as it would be with `>`. A whole page of one value (a batch written in one instant) is read with all the rows of that value, up to 100,000 |
| Which column | one that grows with each new or changed row: a time of change set by the database (`UPDATED_AT`, with fractions of a second) or a growing id. A row changed without changing that column is not read again |
| More rows | a pull that brings a full page pulls again at once, held back while two pages of the table wait in the inbox: a table of a million rows comes in at the speed the inbound handles it, a page at a time, without filling the inbox |
| A row | is a message: its key is the table, its cursor and the hash of its columns, unless the inbound has `key`. A row changed later (a new cursor, other values) is a new message |

Each row is a JSON object of its columns, read as the inbound's record `from json`: a field
reads the column of its `at`, else of its name, exactly as the database names it. A field whose
column is not in the table stops the pull, with the reason in the log of the inbound
(`quadra inbox log 0`), before any row is kept:

```text
the table erp.orders has no column `CUSTOMER_CODE` (the field `customer` of ErpOrder); its columns: ORDER_ID, ORDER_NO, CUSTOMER, TOTAL, UPDATED_AT
```

| In the database | In the row |
|---|---|
| integers | a number; one beyond ±2^53 (9007199254740991) as a string, read the same by an `Int` field |
| `NUMERIC`, `DECIMAL` | a string with the exact digits (`"1234567890.10"`), read by a `Decimal` field without rounding |
| floating point, and SQL Server's `MONEY` | a number (for `MONEY` exact to the cent below about 10^11; cast it to `DECIMAL` in a view for more) |
| boolean, `BIT` | `true` or `false` (MySQL's `BOOLEAN` is a `TINYINT`: `0` or `1`) |
| text | a string |
| `DATE`, `TIME` | `"2026-11-27"`, `"10:14:03.5"` |
| a time without zone (`TIMESTAMP`, `DATETIME`, `DATETIME2`) | `"2026-11-27T10:14:03.123456"`, read as UTC by a `DateTime` field |
| a time with zone (`TIMESTAMPTZ`, `DATETIMEOFFSET`, MySQL's `TIMESTAMP`) | `"2026-11-27T09:14:03Z"`, `"2026-11-27T10:14:03+01:00"` |
| binary | base64 |
| `JSON`, `JSONB` | the JSON itself: a field can be a record `from json` |
| `UUID`, `UNIQUEIDENTIFIER` | a string |
| `NULL` | `null`: an optional field is none, any other fails the message (`rejected`) |
| others (intervals, arrays, geometry) | refused with the column's name: read them through a view that casts them to text |

On ODBC every value comes as the driver writes it, typed by the column (`DECIMAL` as its exact
text, a timestamp `"2026-11-27T10:14:03.123456"`).

### Writing: `insert(table, record)`

```quadra
record ErpOrderRow from json:
    number: Text at "ORDER_NO"
    customer: Text at "CUSTOMER_CODE"
    total: Decimal at "TOTAL"
    placed: DateTime at "PLACED_AT"

event OrderPlaced(order: Order)

outbound OrderPlaced via erp_db as e:
    let o = e.order
    insert("erp.exchange_orders", ErpOrderRow(number=o.number, customer=o.customer.code, total=o.total, placed=o.placed_at))
```

```sql
CREATE TABLE erp.exchange_orders (
    "ORDER_NO" VARCHAR(20) NOT NULL,
    "CUSTOMER_CODE" VARCHAR(20) NOT NULL,
    "TOTAL" NUMERIC(14, 2) NOT NULL,
    "PLACED_AT" TIMESTAMP NOT NULL,
    quadra_key VARCHAR(100) UNIQUE,        -- each event once
    taken_at TIMESTAMP                     -- set by the other system when it takes the row
);
```

| Rule | Detail |
|---|---|
| One row | the block ends with `insert`: one `INSERT` of the record's columns (its fields' `at`, else their names) into the table named in the code, its values bound as parameters and read by the database in each column's type (a `Decimal` exactly, a `DateTime` in UTC). The record is `from json`, flat |
| Each event once | a table with a column `quadra_key` gets the event's idempotency key there. An event sent again (the answer lost, a restart) whose key is already in the table is delivered and not written again; with `UNIQUE` on the column, two attempts at once cannot both write it either. Without the column, an event sent again is a second row |
| Refused | a column the table does not have (with the table's columns in the error), a table that is not there, a value the column does not take: the event fails for good, the reason in its log. A database that does not answer, a timeout, a deadlock: tried again |
| Credentials refused | for good, like a malformed row: correct them and `quadra outbox retry` |

**Only exchange tables.** Write only into tables made for the exchange and agreed with the
other system, which reads them with its own programs, checks them and takes them in; never into
its application tables. A row written there directly skips the rules, the numbering and the
side effects of the system that owns them. On an AS/400 this is the rule without exception:
writing into the files of the application bypasses its RPG programs, so the system's own
checks and updates never happen. Ask for an exchange file (with its library, its columns and
who clears it), and give Quadra's user the right to insert there only.

### ODBC and the AS/400

`kind = "odbc"` reaches any database with an ODBC driver: DB2 for i on the AS/400 above all.
The driver is installed where the connection runs, so an ODBC connection has
`location = "agent:<name>"`: the agent on a machine of the company's network, with unixODBC and
the IBM i Access ODBC Driver, runs its pulls and its inserts. A server built without ODBC
(Quadra Cloud) leaves them waiting, saying so. On ODBC a page is read with `FETCH FIRST n ROWS ONLY`.
A file that is not journaled takes inserts only without commitment control: `CommitMode=0` in
the connection string of the IBM i Access driver.

### Where a database is reached

On Quadra Cloud a connection reaches public addresses only: a database name that resolves to a
private, loopback or link-local address is refused (`… is not a public address`), so is one in
the url. A database inside a company's network, which is where they nearly always are, is
reached through an agent there (`location = "agent:<name>"`), with `insecure = true` when the
database has no TLS: the agent reaches it in its own network, with the credentials it holds.
Connections are kept open between pulls for two minutes.

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
| Options | `encoding` (`"utf-8"`, `"latin1"`, or an EBCDIC code page, below); `length`, the length of a record when it goes on after its last field (else the end of its last field). Lines end with `\r\n` when written |
| Not yet | a file mixing records of several layouts is read one layout at a time; binary numbers (COMP) and the zoned numbers of ASCII files are not read |

**EBCDIC.** The files of mainframes and AS/400, sent as they are (a binary transfer), are in
EBCDIC: `encoding="ibm-280"` (Italy), `"ibm-1144"` (Italy, with `€`), `"ibm-037"` and
`"ibm-1140"` (United States), `"ibm-273"` and `"ibm-1141"` (Germany, Austria), `"ibm-285"`
and `"ibm-1146"` (United Kingdom), `"ibm-297"` and `"ibm-1147"` (France), `"ibm-500"` and
`"ibm-1148"` (international). Then the file is records of bytes back to back, without new
lines, each `length` bytes; columns count bytes. A number may be held in two ways besides
its digits:

| Field | Bytes | Example |
|---|---|---|
| `qty: Int at "13..15" zoned` | a digit a byte, the sign in the first half of the last byte (`C` positive, `D` negative; `F` read as positive) | `-3` in 3 bytes: `F0 F0 D3` |
| `amount: Decimal at "16..19" packed decimals 2` | packed decimal (COMP-3): two digits a byte, the sign in the last half-byte; `n` bytes hold `2n - 1` digits | `1234.56` in 4 bytes: `01 23 45 6C` |

A packed or zoned field of spaces (or of zeros, `low-values`) is none for an optional field.
`rows.fixed()` and `parse_rows(text)` of such a record give and take the text of the bytes,
a character each, so that `CbiRow.parse_rows(rows.fixed()) == rows` holds; to write the file
itself, `write(path, rows)` in an outbound.

```text
record 3, bytes 16..19 (`amount`): not a packed decimal: its last half-byte (0x7) is not a sign (C, D or F)
```

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
| Egress | connections reach public addresses only: what is in another network goes through an [agent](#on-premise-agents) |
| Who | people change and read values; a project's token (CI) cannot |

## On-premise agents

A database, an SFTP server or a cash register in a company's network is behind its
firewall: Quadra cannot reach it, and the company does not want to open a port. An **agent**
is a program installed in that network (`quadra agent`, the same binary as `quadra`, on Linux,
Windows or macOS) that opens connections **only outwards**, to the app, takes the jobs of the
connections it runs, and sends back what came of them.

```toml
[connections.erp]
kind = "postgres"
url = "postgres://erp.lan:5432/gestionale"   # an address of that network
user = "quadra"
password = { var = "ERP_PASSWORD" }
location = "agent:milano"                    # the agent `milano` runs it
```

A connection of any kind runs on an agent with `location = "agent:<name>"`: a database, files
over SFTP or FTP, a mailbox, an HTTP service of the network. The app plans the jobs (a pull
due, an event to deliver) as for any connection; the agent runs them.

**Its credentials stay with the agent.** The job carries the connection as declared in
quadra.toml, never the values of its variables: the agent reads them from its own environment
(and the `.env` next to its config). A variable used only by connections that agents run is not
needed where the app runs (`quadra cloud env set` is not asked for it).

### Install and register

In the studio, **Integrations → Agents → Add an agent** (an admin) gives a one-time code, valid
for an hour. On a machine of that network:

```bash
quadra agent run --server https://shop.quadra.run --code K7QD-9XMP-2RTA
```

The agent registers with the code and keeps its key in `quadra-agent.toml` in the folder it
runs in, readable by its user only; the code cannot be used again. Then `quadra agent run` in
that folder, as a service of the system (systemd, a Windows service, launchd), whenever the
machine starts. Several agents for one project (one per site), each with its name.

| Rule | Detail |
|---|---|
| Connections | only outwards, HTTPS to the app: it asks for its jobs (`GET /v1/agent/jobs`, held open up to 20 s) and sends back what came of them |
| Kept | only the SHA-256 of the code and of the key, never them |
| Status | the studio shows each agent connected or not, its version and when it was last seen |
| Not connected | a job waits a little for it (45 s), then fails as a connection that does not answer would: tried again later |
| Revoked | **Revoke** in the studio (or `quadra agent revoke <name>`): it receives nothing more within 5 s, its key no longer valid; registering the name again needs a new code |
| Quadra Cloud | an app asleep is not woken by its agent asking for jobs: it wakes when a pull or a delivery falls due, and the agent gets the job at its next request (within 20 s) |
| Upgrade | `quadra agent upgrade` (the latest release of `quadra`) |
| ODBC | the drivers of ODBC (DB2 for i, the AS/400) are installed where the agent runs, not in the cloud |

## In tests

Tests do not read the environment, so that they give the same result everywhere: `env.NAME`
is the variable's `test` value, else its default; reading a variable that has neither fails
the test. Tests send nothing and receive nothing: `receive` plays a message coming in,
`expect sent` checks what was emitted, and `deliver` plays the other side's answer
([Testing](/reference/testing#integrations)).

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
