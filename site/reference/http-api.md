# HTTP API

`quadra serve` exposes one project and its database over HTTP. There is no code to write per
entity: the model (entities, fields, actions, flows, permissions) defines the API. Everything
is JSON, every request runs as an authenticated user with that user's permissions, and every
write is one atomic, idempotent transaction.

```bash
quadra serve examples/extended --url sqlite:/tmp/demo.db --port 8091 --dev-login
```

| Endpoint | Purpose |
|---|---|
| `GET /v1/me` | the current user |
| `GET /v1/model` | the entities the user may read, with fields, actions and flows |
| `POST /v1/query` | rows of an entity, filtered, ordered, with computed columns |
| `GET /v1/records/{Entity}/{id}` | one record, with what the user may do on it now |
| `GET /v1/records/{Entity}/{id}/history` | every write of the record, with its cause |
| `GET /v1/search/{Entity}` | records found by their number, code, name or login, then by full-text search (pickers) |
| `POST /v1/tx` | a transaction: creates, updates, actions |
| `GET /v1/stream` | live events, one per committed transaction |
| `POST /in/<connection>[/<path>]` | a message for an `inbound`, signed by the sender ([Variables and connections](/reference/connections#inbound-messages-in)) |
| `GET`/`POST /in/<connection>/<path>` | an `endpoint`, answered at once ([Variables and connections](/reference/connections#endpoint-answered-at-once)) |
| `GET /v1/integrations`, `/v1/integrations/outbox[/{id}]`, `/v1/integrations/inbox[/{id}]`, `POST …/{id}/retry`, `POST …/{id}/skip {"reason"}` | what went out and came in, by outbound and inbound (counts per state, the oldest waiting), each message with its log, retried or skipped with who and why, and the connections (as `GET /v1/connections`): the studio's page "Integrations". For those who may edit `User.roles` |
| `GET /v1/connections`, `GET /v1/connections/{name}/authorize`, `POST /v1/connections/{name}/disconnect` | the connections, and for one with OAuth2 consent whether it is connected, its scope, expiry, who connected it and when (never its tokens); the redirect that starts the consent; its tokens forgotten. For those who may edit `User.roles` ([OAuth2 with the user's consent](/reference/connections#oauth2-with-the-users-consent)) |
| `GET /oauth/{connection}/callback?code=…&state=…` | where the provider sends the admin back after the consent: no session, the sealed `state` is the proof; answers a page that says whether it is connected |
| `GET /v1/outbox`, `POST /v1/outbox/ack`, `POST /v1/outbox/nack` | the events no `outbound` delivers, taken with a lease and settled by an integration ([Variables and connections](/reference/connections#without-outbound-an-integration-takes-them)) |
| `POST /v1/auth/dev-login`, `POST /v1/auth/logout` | development sessions |
| `/v1/dev/…` | the code of the project, for the browser editor |

## Authentication

| Who | How |
|---|---|
| Programs | `Authorization: Bearer qd_t_…`, a token created with `quadra token create <login>` ([CLI](/reference/cli#api-tokens-quadra-token)). Tokens can be read-only and expire; only their SHA-256 hash is stored. |
| Browsers | A session cookie `quadra_session` (`HttpOnly; SameSite=Strict`, `Secure` with `--secure-cookies`, 12 hours). Every request other than `GET` must send the session's CSRF token in `X-CSRF-Token`, also `POST /v1/query` (a read sent as `POST`), and requests whose `Origin` is another site are refused. Bearer tokens need no CSRF token. |
| Development | With `--dev-login`, `POST /v1/auth/dev-login {"login": "giulia"}` opens a session for that user without a password and answers `{"user", "csrf"}`. Without the flag the endpoint answers `404`. |

`POST /v1/auth/logout` ends the session.

`GET /v1/me` answers `{"user": "mario", "user_id": 22, "read_only": false, "csrf": null, "kind": "token"}`.

### Limits

| Limit | Value |
|---|---|
| Request body | 2 MiB |
| Requests | 30 per second per credential, bursts up to 120 (`429 too many requests`) |
| Rows per query page | 1000 (200 by default) |
| Operations per transaction | 1 to 200 |
| Expressions in queries | one line, at most 2000 characters |

## Reading

### `GET /v1/model`

The entities the user may read (temporary entities, records and traits excluded), with:

| Key | Content |
|---|---|
| `name`, `module`, `ledger`, `traits` | the entity |
| `can_create` | whether the user may create records |
| `frozen_when` | the `frozen when` condition, as text |
| `fields` | for each visible field: `name`, `kind` (`text`, `int`, `decimal`, `percent`, `money`, `price`, `qty`, `date`, `datetime`, `duration`, `bool`, `enum`, `ref`, `record`, `list`, `set`, `role`, `other`), the currency or unit (`currency`, `unit`, `per`, or `currency_from`… when it comes from another field), `variants` of an enum, `entity` of a reference, `formula`, `required`, `optional`, `has_default`, `editable`, `parent`, `per_company` |
| `actions` | the actions the user may run, with their parameters (`name`, `type`, `default` as written, `optional`) |
| `flows` | each flow's field and transitions (`from`, `to`, `by`, `automatic`) |

The answer also has `base`, the latest transaction.

### `POST /v1/query`

```json
{
  "entity": "SalesOrder",
  "fields": ["number", "customer.name", "total"],
  "where": "state is Confirmed and total > 1000 EUR",
  "columns": { "margin_pct": "margin / untaxed" },
  "order_by": [{ "field": "total", "desc": true }],
  "limit": 200,
  "after": "…the next cursor of the previous page…",
  "aggregates": { "total": "sum" },
  "base": 1234
}
```

| Key | Meaning |
|---|---|
| `entity` | required; `404 unknown entity` if it does not exist or is not an entity |
| `fields` | field paths to return (at most 100), following references with `.`; default: every non-collection field not hidden from the user |
| `where` | a Quadra expression on the record, like a formula |
| `columns` | computed columns: name (letters, digits, `_`, at most 40) → Quadra expression |
| `order_by` | up to 5 field paths, each ascending or `desc` |
| `limit` | rows per page: at most 1000, 200 by default |
| `after` | the `next` cursor of the previous page (opaque): the page that follows it |
| `offset` | rows to skip (without `after`) |
| `count` | `false`: no `total` (the pages after the first) |
| `aggregates` | `{"<field path>": "sum" \| "count" \| "min" \| "max" \| "avg"}` over every matching row |
| `ids` | restrict to these record ids (at most 1000): a selection |
| `refs` | `{"<field>": id}`: the rows whose reference (or set) holds that record, the lines of a record |
| `search` | full-text search: every word must begin a word of a stored text field the user may see (see below) |
| `group_filter` | `[{"field", "bucket"?, "value"}]`: the rows of a group (a group's `filter`) |
| `filters` | `[{"path", "op", "value"}]`: conditions on field paths (an app's global filters): `op` `eq`, `in` (a list), `range` (`[from, to]`, either null; dates and numbers), `contains` (text), `true` (yes/no); paths through references; a hidden field is refused (`403`); in SQL with the rest |
| `group_by` | 1 or 2 keys: the answer is a page of groups (see [Groups and pivots](#groups-and-pivots)) |
| `base` | read the data as it was after that transaction; pages of one listing should use the same `base` |

`where` and `columns` are type-checked like source code, as formulas of the entity. They may
only read: an expression that would add anything else to the model is refused. A type error
answers `400 {"error": "the expression does not check", "diagnostics": ["cannot add Money<EUR> and Qty<kg>"]}`.
`where` reads the record's current values (a frozen record's formulas keep their frozen
values, as its cells show them).

On a database, the checked `where` and the order are compiled to parameterized SQL over the
tables of the entities: paths through references become joins, `lines.any(…)`, `count(…)` and
sets become `EXISTS` / `COUNT` subqueries, values that do not depend on the row (a literal,
`me`, a constant) are computed first and bound as parameters. The SQL text is made only of
the schema's identifiers and fixed keywords, never of the request's text. The translation
follows the language, not SQL: `!=` and `not` over none, comparisons of amounts and
quantities (one currency, one unit), text compared byte by byte. What SQL cannot decide exactly
the same way — a formula, a function, a comparison that may meet none or another currency,
the row permissions — the engine decides on the rows SQL returns, reading further until the
page is full. The answer is the one the engine gives evaluating every record.

Only the rows the user may read are returned. The order is that of `order_by` (numbers by
value, text by its ASCII-lowercase form, none last — first when descending), then the id, in
the direction of the first key.
`next` is the cursor of the following page, `null` after the last one: a cursor continues
exactly where the page stopped, whatever is added meanwhile.

Only the rows the user may read are returned. The answer:

```json
{
  "entity": "Product", "base": 5, "total": 2, "offset": 0,
  "fields": ["code", "list_price", "type"], "columns": ["double"],
  "rows": [
    { "id": 26, "version": 4, "frozen": false, "warnings": [], "display": "C",
      "cells": {
        "code":       { "v": "C", "d": "C" },
        "list_price": { "v": { "money": "40.00", "cur": "EUR" }, "d": "40.00 EUR" },
        "type":       { "v": { "variant": "Service", "fields": [] }, "d": "Service" },
        "double":     { "v": { "money": "80.00", "cur": "EUR" }, "d": "80.00 EUR" }
      } }
  ]
}
```

`total` counts every matching row the user may read, exactly up to 10 000 of them; beyond
that the answer has `"estimated": true` and `total` is an estimate (the table's rows without a
filter — counted on SQLite, from the planner's statistics on PostgreSQL —; with a filter or row
rules the planner's estimate on PostgreSQL, the share of the table the first 10 001 matches
spanned on SQLite, at most the rows the user may read). `aggregates` answers
`{"<field>": {"fn": "sum", "v": …, "d": "…"}}` (or `{"fn", "error"}`, `{"fn", "hidden": true}`):
none is skipped, a sum needs one currency or unit, decimals come in canonical form; computed
in SQL for stored fields, by the engine on up to 10 000 rows otherwise.

**Full-text search.** `"search": "rossi logis"` keeps the rows where every word of the text
(runs of letters and digits; case ignored, accents kept) begins a word of one of the entity's
stored text fields — `Text`, `Code`, `Email`, `Phone`, `VatNumber`, `Barcode`, not formulas,
not `per company` — that the user may see: a field hidden from the user is never searched, and
only readable rows come back. It combines with `where`, totals, aggregates and groups, and
runs on a search index ([storage](/reference/storage#full-text-search)).

### Groups and pivots

With `group_by` the answer is a page of groups:

```json
{
  "entity": "Order",
  "where": "state in {Confirmed, Shipped}",
  "group_by": ["customer.region", { "field": "date", "bucket": "month" }],
  "aggregates": { "total": "sum" },
  "group_order": { "by": "total", "desc": true },
  "limit": 100
}
```

| Key | Meaning |
|---|---|
| `group_by` | 1 or 2 keys: a field path (through references, at most 4 relations) to a reference, text, a choice, an integer, a yes/no or a date; a date may have a `bucket`: `day` (default), `week` (starting Monday), `month`, `quarter`, `year`. A hidden field: `403`; an amount, a quantity or a decimal: `400` |
| `aggregates` | as for rows, per group; a sum over several currencies (or units) in a group is `{"fn", "error": "several currencies in this group"}` for that group |
| `group_order` | `{"by": "key" \| "count" \| "<an aggregate's field>", "desc": false}`: by key (as cells are ordered, none last) by default; not by an `avg`; ties by the keys |
| `pivot` | `true` with two keys: the second key's values become columns (at most 50) |
| `limit`, `after`, `offset` | groups per page (at most 1000, 100 by default) |

`where`, `search`, `group_filter`, `ids`, `refs` and the user's row permissions apply. The answer:

```json
{
  "group_by": [{ "field": "customer.region", "bucket": null, "kind": "ref", "entity": "Region" },
               { "field": "date", "bucket": "month", "kind": "date", "entity": null }],
  "groups": [{
    "keys": [{ "value": "12", "d": "North" }, { "value": "2026-03", "d": "2026-03" }],
    "count": 340,
    "aggregates": { "total": { "fn": "sum", "v": { "money": "51000.00", "cur": "EUR" }, "d": "51,000.00 EUR" } },
    "filter": [{ "field": "customer.region", "bucket": null, "value": "12" },
               { "field": "date", "bucket": "month", "value": "2026-03" }]
  }],
  "groups_total": 660, "next": "…", "sql": true
}
```

A key's `value` is text — the record's id, the text or choice, `true`/`false`, the date's
bucket (`2026-03-05`, `2026-03-02` for a week, `2026-03`, `2026-Q1`, `2026`) — or `null` for
the rows without a value; `d` is what to show. `filter` is the `group_filter` of the group's
rows: send it with a query to list them. With `pivot`, each group also has `cells` (one per
column, `null` when empty) and the answer has `columns` (each column's key, totals and
`filter`), `more_columns` and `columns_total`.

The groups are computed by one `GROUP BY` in the database, the row permissions, `where` and
`search` included (`"sql": true`). When the query does not translate — a condition, a
permission or a formula SQL cannot decide exactly, a key through records the user may not all
read (their values are hidden) — the engine groups the rows itself, up to 10 000 of them;
beyond that the answer is `422` (filter further), never groups of part of the rows.

`display` is the record as people read it (its `number`,
`code`, `name` or `login`). A cell is `{ "v": value, "d": "display text" }`; a hidden field, or
a path through a record the user may not read, is `{ "hidden": true }`; a value that fails to
compute is `{ "error": "…" }`.

### Values

| Value | JSON (`v`) |
|---|---|
| text, integer, boolean, none | `"text"`, `42`, `true`, `null` |
| `Decimal` | `{ "d": "1.5" }` |
| `Percent` | `{ "pct": "0.22" }` |
| `Money` | `{ "money": "36.60", "cur": "EUR" }` |
| `Qty` | `{ "qty": "3", "unit": "pcs" }` |
| `Price` | `{ "price": "10", "cur": "EUR", "per": "pcs" }` |
| `Date`, `DateTime` | `{ "date": "2026-11-21" }`, `{ "dt": "2026-11-21T08:00:00Z" }`: an instant, always in UTC with its `Z` |
| `Duration` | `{ "dur": seconds }` |
| year-month, `DateRange` | `{ "ym": "2026-11" }`, `{ "range": ["2026-11-01", "2026-11-30"] }` |
| enum value | `{ "variant": "Confirmed", "fields": [] }` |
| reference | `{ "ref": 71, "ent": "SalesOrder" }` |
| record value | `{ "rec": "Address", "fields": [["city", "Milano"], …] }` |
| list, set, tuple | `{ "list": [...] }`, `{ "set": [...] }`, `{ "tuple": [[name, value], …] }` |

### `GET /v1/records/{Entity}/{id}`

One record, as of `?base=N` if given:

| Key | Content |
|---|---|
| `entity`, `id`, `display`, `version`, `base`, `frozen`, `warnings` | the record |
| `cells` | every field the user may see; a collection computed from the other side (the lines of a document, a region's `customers`) bounded, see below |
| `editable` | the fields the user may change now (not formulas, not flow fields, not frozen unless in `except`; the lines of a frozen document are frozen with it) |
| `actions` | the actions the user may run now: permission and current state (for an action tied to a flow, with `to`, the state it leads to) |
| `unavailable` | the other actions and transitions, each with its `reason`: the permission refused, or `only from Draft (it is Confirmed)` |
| `cause` | the chain of causes of the last change |
| `history` | whether the user may read the history |

The entity in the URL may be a trait the record implements. `404` if the record does not
exist, `403` if the user may not read it.

A collection computed from the other side comes as a page: its first 50 records the user may
read, in id order (`v`, `names`), how many there are (`count`; `"estimated": true` beyond
10 000) and `next`, the cursor of the next page (`null` when there is none):

```json
"customers": { "v": { "list": [{ "ref": 71, "ent": "Customer" }, …] }, "names": ["C0001", …],
               "d": "C0001, C0002, C0003 … (1000)", "count": 1000, "next": "7b226b…" }
```

The next pages are `POST /v1/query {"entity": "Customer", "refs": {"region": 12},
"after": "<next>"}`. A collection the record stores (`team: {User}`) or computes with a
formula comes whole.

### `GET /v1/records/{Entity}/{id}/history`

Every write of every field, oldest first: `{"tx", "at", "user", "cause", "field", "value"}`.
Fields renamed by a migration appear under their current name; hidden fields are left out.
Needs `read history` (or `all`) on the entity: otherwise `403`. With `?limit=50`, the 50 newest
entries (oldest first among them) and `before`: pass it as `?before=` for the older ones
(`null` when there are none).

### `GET /v1/search/{Entity}`

`?q=rossi&limit=20&after=…`: the records a reference picker offers. First those one of whose
display fields — `number`, `code`, `name`, `login`, the stored text ones the user may see —
starts with `q` (ASCII case-insensitive, an index search), then those matching the full-text
search of `q` (every word begins a word of a stored text field the user may see); each group
by its display text and id. Only the records the user may read.

```json
{ "entity": "Customer", "rows": [{ "id": 12, "name": "K0000012" }], "next": "…" }
```

### `GET /v1/stream`

Server-sent events. For each committed transaction (from this server or any other process on
the same database), an event `commit`, with the transaction number as `id`:

```text
event: commit
id: 7
data: {"tx":7,"at":"2026-11-22T00:03:00","user_id":22,"cause":"API request k7  (mario, 2026-11-22 00:03)","records":[{"entity":"Product","id":24}]}
```

`records` lists the records written, limited to the entities the user may read; a
transaction with none of them sends nothing. `?since=N` first replays the transactions after
`N` (up to 100), then goes on live: each transaction is delivered once, in order (a client
that reconnects passes the last `id` it received as `since`). After a deploy from the code
editor, an event `deploy` (`{"type": "deploy", "sha": "…"}`) tells clients to reload the
model. When a saved view or a team is created, changed, shared or deleted, an event
`views` (`{"type": "views", "entity": "Order"}`) tells clients to read their list of views
again ([Views and teams](#views-and-teams)).

## Writing

### `POST /v1/tx`

```http
POST /v1/tx
Authorization: Bearer qd_t_…
Idempotency-Key: order-2026-11-21-0042
Content-Type: application/json
```

```json
{
  "base": 1234,
  "ops": [
    { "op": "create", "entity": "SalesOrder", "ref": "so",
      "values": { "customer": 59, "lines": [{ "product": 63, "qty": "3 pcs" }] } },
    { "op": "update", "entity": "SalesLine", "id": 70, "values": { "qty": "5 pcs" } },
    { "op": "action", "entity": "SalesOrder", "id": 71, "action": "confirm" },
    { "op": "action", "entity": "Invoice", "id": 80, "action": "credit_note",
      "args": { "reason": "Returned goods" } }
  ]
}
```

| Operation | Keys |
|---|---|
| `create` | `entity`, `values`, and optionally `ref`: a name for the new record, usable by later operations (`{"ref": "so"}`) and returned in `created` |
| `update` | `entity`, `id`, `values` |
| `action` | `entity`, `id`, `action`, and optionally `args` (named parameters) |

All the operations are **one transaction**: everything is written or nothing is. Rules are
checked at the end, as for any transaction ([Rules](/reference/rules#when-rules-are-checked)).

Values in `values` and `args`:

| Value | Written as |
|---|---|
| a reference | the record's id (`59` or `{"id": 59}`), or `{"ref": "so"}` for a record created earlier in the same request |
| lines (children) | objects, inline: `"lines": [{"product": 63, "qty": "3 pcs"}]` |
| a `record` value (an address) | an object of its fields: `{"street": "Via Roma 1", "city": "Milano", …}` |
| lists and sets | JSON arrays of the element values |
| amounts, quantities, prices, percents, dates, durations | Quadra literals as text: `"5.40 EUR"`, `"4 pcs"`, `"1.50 EUR/pcs"`, `"10%"`, `"2026-11-21"`, instants with their zone `"2026-11-21T09:30:00+01:00"` or `"2026-11-21T08:30:00Z"` (without one, `"2026-11-21T09:30"` is a time of the project's clock: send the zone when the time comes from another system); durations as people write them: `"8 h"`, `"1h30"`, `"1:30"`, `"90 min"`, `"2 days"`, `"1 day 4 h"` (a number alone is refused: hours or days?) |
| enum values | the variant's name: `"Storable"` (otherwise `` `X` is not one of Storable, Consumable, Service ``) |
| text, numbers, booleans, null | JSON values (text at most 10,000 characters) |

Literals are read by fixed rules and never evaluated. Amounts are checked against the field:
another currency is refused (`Product.list_price: expected an amount in EUR, not USD`),
another unit of the same kind is converted (`"2000 g"` becomes `"2 kg"`), another kind is
refused. A bare number takes the field's currency or unit.

### Concurrency: `base` and 409

`base` is the transaction the client read the data at (every read answers its `base`). It is
required as soon as an operation changes an existing record. If someone else changed one of
the records the request changes (a stored field of a record it updates or runs an action
on) after `base`, nothing is written: the request would overwrite a change its sender never
saw.

```json
409
{
  "error": "changed by others since you read it: review their changes and try again",
  "conflicts": [
    { "tx": 6, "at": "2026-11-22T00:02:49", "user": "mario",
      "cause": "API request k1  (mario, 2026-11-22 00:02)", "what": ["Product#29.code"] }
  ]
}
```

Show the conflicts to the user, read again, and send a new request (with a new key) from the
new `base`.

Everything else the transaction relies on (totals, rollups, the records its rules read) is
read when it runs, on the latest data, and its rules are checked on that: a credit limit or
a stock level holds even if others committed since `base`. Two people changing two lines of
the same order both succeed. If another commit lands while the transaction runs, it runs
again on it by itself (a few times at most, then `409`). See
[Storage](/reference/storage#optimistic-concurrency).

### Idempotency

`Idempotency-Key` is required: 1 to 100 characters among letters, digits and `- _ : .`.

| Request | Answer |
|---|---|
| a new key | the transaction runs |
| the same key and the same body | the first answer, with `"replayed": true`; nothing runs again |
| the same key and a different body | `422 this Idempotency-Key was already used for a different request` |

The key is stored in the same database transaction as the commit, so a retry after a network
error is always safe. Keys are per user.

### Answers

| Status | When |
|---|---|
| `200 {"tx", "base", "created": {"so": 71}}` | committed |
| `400` | malformed request, missing `Idempotency-Key`, missing `base` |
| `401` | not authenticated, or an expired or revoked credential |
| `403` | not allowed, with the rule's message (`permission denied: giulia cannot run SalesOrder.lock`), or a read-only token |
| `409` | conflict |
| `422` | refused by the model (a `check`, a `require`, a frozen record, the field of a flow, a wrong value), with its message: ``Product.hs_code = 1: does not satisfy its `where` `` |

## Views and teams

Saved views and boards are data users create in the web client, not code
([Views and boards](/reference/views-and-boards)). A view never grants data: it is a
configuration, run by `POST /v1/query` as whoever opens it. Every write below needs the CSRF
header like any other non-`GET` request; a read-only token may only read.

| Call | What it does |
|---|---|
| `GET /v1/views?entity=Order[&kind=view][&scope=all][&favourite=1]` | the views the user may open (their own, and those shared with them: as a user, through a team, through a role or an inherited role, with everyone), of the entities they may read, in the user's order then by name; `scope=all` (admins only): every view; `favourite=1`: only the user's favourites (without `entity`: those of every entity, in one answer). With `me: {id, admin, roles, teams}` |
| `GET /v1/views/{id}` | a view (`404` when the user may not open it) |
| `POST /v1/views {entity, type, name, description?, config, shares?}` | a new view (version 1), owned by the user: `201 {view}` |
| `PUT /v1/views/{id} {base, name?, description?, type?, config?, owner?}` | change it: its owner, co-owners (`edit`), admins; `owner` only the owner or an admin |
| `PUT /v1/views/{id}/shares {base, shares}` | who it is shared with (owner, admins) |
| `DELETE /v1/views/{id}?base=N` | delete it (owner, admins): marked deleted, its history kept |
| `PUT /v1/views/{id}/prefs {favourite?, default?}` | the user's own: a favourite, the view opened first for its entity (one per entity) |
| `PUT /v1/views/order {ids}` | the user's order of views (of apps and folders inside a folder too: positions are compared among neighbours only) |
| `GET /v1/views/{id}/history` | every change, newest first: `{version, at, user, action, changed, state}` (`user: null`: a deploy) |
| `GET /v1/teams` · `POST /v1/teams {name, description?, members?}` | teams; admins create them (the creator is an owner unless owners are given) |
| `GET /v1/teams/{id}` · `PUT /v1/teams/{id} {base, name?, description?, members?}` · `DELETE /v1/teams/{id}?base=N` | a team; changed by admins and its owners. `members: [{id, owner}]` replaces the members |
| `GET /v1/teams/{id}/history` | every change of a team |
| `GET /v1/directory?q=` | whom to share with: users whose login or name starts with `q` (at most 50), teams, roles (with the role they extend), and `me` |

Admins are the users who may edit `User.roles` (in a model without roles, everyone).

A view: `{id, kind: "view", entity, type: "grid" | "groups" | "kanban", name, description,
config, version, owner: {id, login, name}, created_at, created_by, updated_at, updated_by,
access: "owner" | "admin" | "edit" | "use", can_edit, can_share, shares, favourite, default,
position, broken}`. `shares`: `[{type: "user", id, edit}, {type: "team", id, edit},
{type: "role", name, edit}, {type: "everyone", edit}]` (`edit`: a co-owner). `kind` tells
views apart from apps (below), which share the same storage and sharing.

`config` is what the client shows: `fields` (paths), `widths`, `where`, `order`
(`[{field, desc}]`), `columns` (personal columns: name → expression), `colors`
(`[{expr, color}]`), `search`, `group` (`{keys, aggregates, pivot, order, measure}`),
`grouped`, `drill`, `board`, `kanban` (`{field, title, cards, hidden, collapsed, wip,
aggregate}`), at most 64 KB. When saved, its expressions are checked as those of a query and
its paths against the model: `400 {"error": "the view does not check", "diagnostics":
["where: unknown name `totl`", "column `customer.nme`: `Customer` has no field `nme`"]}`;
`where` and color rules must be yes or no.

Changes are made from the `version` read (`base`): when someone changed the view or the team
since, nothing is written and the answer is `409 {"error", "conflicts": [{"version", "at",
"user", "cause", "what": ["config.where"]}]}`. `broken`: what no longer fits the model since a
deploy (the checker's messages), `null` otherwise; saving a configuration that checks clears it.
After a deploy the server renames, in every view, the fields a migration renamed (paths and
expressions; recorded as `deploy: renamed …`) and marks broken the views using what no longer
exists. Every successful write sends a `views` event on `GET /v1/stream` (clients read their
list again).

## Apps and formulas

Apps (dashboards: pages of components on a grid) are kept and shared exactly as views, with
`kind: "app"` ([Apps and dashboards](/reference/apps-and-dashboards)): `GET /v1/views?kind=app` lists those the user may
open, `POST /v1/views {kind: "app", name, description?, config, shares?}` makes one (`type` and
`entity` are `"app"` and `null`), and `PUT /v1/views/{id}`, `/shares`, `/prefs`, `/history`,
`PUT /v1/views/order`, `DELETE` work as for views (409 with the conflicts on a stale `base`).
An app never grants data: what it shows is computed as whoever opens it.

`config` (at most 1.5 MB): `{version, pages: [{id, name, components: [{id, type, title?,
layout: {x, y, w, h}, config, bindings}]}]}` on a grid of 24 columns. The server checks every
component's envelope (unique ids of letters, digits, `-` and `_`; `type` in lower case; `0 ≤ x`,
`1 ≤ w ≤ 24`, `x + w ≤ 24`, `1 ≤ h ≤ 200`) and the shape of the `config` of the types it knows:
`sheet` (a workbook, below), `markdown` (`{text}`), `chart` (`{kind: bar|line|area|pie|scatter,
stacked?, horizontal?, legend?, source: {type: "expr", expr} | {type: "range", component,
range}, max_points?, x_title?, y_title?, format?}`), `kpi` (`{source: {type: "expr", expr} |
{type: "cell", component, cell}, label?, format?: {style, decimals}, compare?: {kind:
"previous_period", filter} | {kind: "target", expr}, higher_is_better?, sparkline?}`), `filter`
(`{filters: [{name, label?, entity, field, widget: select|multi|date_range|number_range|toggle|text,
default?}], show?, layout?}`), `view` (`{entity, view_id?, type?: grid|groups|kanban, view?,
select?, page_size?, edit?: {cells?, add?, move?}}` — read-only without `edit`), `button` (`{label, entity, action, target, args?, confirm?, style?}`),
`form` (`{entity, mode: create|edit, record?, sections: [{title?, fields}], columns: 1-3,
defaults?, submit_label?}`, or a custom form `{mode: "custom", fields: [{name, type, label?,
where?, default?}], submit, submit_label?, confirm?, after?: clear|keep|open|select, select?:
{name, entity}, message?, columns?}`: below) — a record or a value comes `{from: "param", name}`, `{from: "id",
id}`, `{from: "cell", component, cell}` or `{from: "value", value}`. A component's `bindings`:
`{filters: {auto?: bool, "<Entity>": {"<filter>": "<path>" | null}}}`. The names of filters and
of selected records (`select`) are unique in the app. The `config` of other types is kept as it
is. Entities, fields, actions and formulas are checked when they are used, as the user.
`400 {"error": "the app does not check", "diagnostics"}` otherwise.

A workbook (the `config` of a `sheet`): `{active, sheets: [{id, name, mode: "fixed" | "free",
rows, cols, cells: {"A1": {v, s}}, cols_w: {"A": px}, rows_h: {"3": px}, merges: ["A1:C1"],
freeze: {rows, cols}}]}`: `v` is what was typed (a value, or a formula starting with `=`), `s`
its style (`b`, `i`, `u`, `st`, `fg`, `bg`, `ha`, `va`, `wrap`, `fmt: {kind, dec, thou, cur,
date}`, `bd: {t, b, l, r, c}`).

`POST /v1/eval` — the formulas of a workbook, as the user (a read sent as `POST`: read-only
tokens may call it):

```json
{ "sheets": [{ "id": "s1", "name": "Sheet1", "cells": { "A1": { "v": "R01" }, "B1": { "v": "=Customer.count(c => c.region.code == A1)" } } }],
  "exprs": { "k1": "Sheet1!B1 * 2" },
  "changed": ["s1!A1"] }
```

The app's filters and selected records come as `params` (every one, set or not) and the
entities they apply to as `bind`:

```json
{ "exprs": { "n": "Order.count()", "mine": "Order.count(o => o.customer.region in @region)" },
  "params": [{ "name": "region", "entity": "Order", "field": "customer.region", "widget": "multi", "value": [12] },
             { "name": "period", "entity": "Order", "field": "date", "widget": "date_range", "value": ["2026-01-01", null] },
             { "name": "order", "entity": "Order", "widget": "record", "value": 3154 }],
  "bind": { "Order": { "region": "customer.region", "period": "date" }, "OrderLine": { "region": "order.customer.region" } } }
```

A parameter is a typed parameter of the formulas, `@name`, its type from the field (`select`:
`T?`; `multi`: `[T]`; `record`: `Entity?`; `date_range`: `DateRange`; `number_range`:
`@name_min` and `@name_max`, `T?`; `toggle`: `Bool`; `text`: `Text`): changing a value does not
check the formulas again. `bind` makes every `Entity.…` of the formulas the rows that pass the
bound filters (a filter not set holds for every row), in SQL with the rest. `400` when a
parameter or a path does not fit the model; `#REF` for an unknown `@name`.

The answer: `{ "base", "ms", "reads": ["Customer", …], "partial", "cells": { "s1!B1": Cell },
"exprs": { "k1": Cell } }` — every formula's result (with `changed`: only the cells changed and
those using them, `partial: true`). `Cell`: `{v, d, kind}` (values as in queries; `kind`:
`int`, `decimal`, `percent`, `money`, `qty`, `price`, `date`, `datetime`, `text`, `bool`, `ref`,
`list`, `tuple`, `none`…), or `{code, error}` with `code` one of `#TYPE` (the checker's message),
`#CYCLE`, `#REF`, `#SPILL`, `#HIDDEN`, `#NOACCESS`, `#TIMEOUT`, `#ERROR`; `spill: {rows, cols,
cells: [[{v, d, kind} | null]], total, truncated, header?}` when the value is a list (at most
10 000 rows shown; `header`: the names of named tuples; an expression's list too); `certified: "<metric>"` when the formula is a call of a metric; `db: true` when it
reads the database. `reads`: the entities the formulas read — a `commit` event of one of them is
when to ask again. `exprs` are evaluated like cells (`{= … }` of a markdown text). A request
takes at most 20 seconds (`timeout_ms` may ask for less): the formulas still running show
`#TIMEOUT`. Formulas reading the database are kept between requests (per user) until a commit
changes an entity they read.

`POST /v1/complete` — completion of a formula (the formula bar, a cell, a chart's or a KPI's
expression, a `{= … }` of a text), in the context `/v1/eval` checks it in, as the user (a read
sent as `POST`: read-only tokens may call it). Nothing is evaluated and nothing is checked
again: the text before the caret is typed with the model's checked types, the app's parameters
(typed by what they filter) and the cells it names (typed by their values or their own
formulas); a request takes about a millisecond on a small model.

```json
{ "text": "=Order.where(o => o.cu", "pos": 22, "kind": "formula",
  "sheets": [{ "id": "s1", "name": "Sheet1", "cells": { "A1": { "v": "5.40 EUR" } } }], "sheet": "s1",
  "params": [{ "name": "region", "label": "Region", "entity": "Order", "field": "customer.region", "widget": "multi" }] }
```

`pos`: the caret, in UTF-16 code units (a JavaScript string index); `kind`: `formula` (a cell's
input: completed after its `=`), `expr` (the whole text is an expression) or `template` (only
inside `{= … }`); `sheets`: as `/v1/eval` takes them, or just their names (`["Sheet1"]`: cells
are then not typed); `sheet`: the formula's sheet (id or name); `params`: as `/v1/eval` takes
them (values are not read; `label` is shown in the item's doc). The answer:

```json
{ "from": 20, "to": 22, "receiver": "Order",
  "items": [{ "label": "customer", "kind": "field", "detail": "Customer", "doc": "`customer: Customer`", "insert": "customer" }],
  "signature": { "label": "where(x => cond)", "params": ["x => cond"], "active": 0, "doc": "the elements that satisfy `cond`" } }
```

`items` replace `[from, to)` (UTF-16) with `insert`, best first (at most 100): after a `.` the
fields of a record (with their types, through references), the methods of a collection (with
their signature) and the fields of its elements, the parts of a value (`date.month`, a text's
methods, a tuple's names: `g.key`, `g.items` of `group_by`), the variants of an enum
(`Level.`); after `@` the app's parameters with their types; a name: lambda parameters in scope
(typed by their collection), parameters, sheets (`Sales!`, `'My sheet'!`), entities, enums and
their variants (those a comparison expects first: `o.state is `, `== `, `in {`), functions and
metrics (with their signature), constants, `today()`, keywords. `kind`: `field`, `formula`,
`method`, `variable`, `param`, `sheet`, `entity`, `enum`, `variant`, `function`, `metric`,
`constant`, `keyword`. `signature`: the call the caret is in (`active`: the argument), or null.
`receiver`: the type of the value before the `.`. Entities the user may not read and fields
hidden from them are not offered. `400`: no `text`, an unknown `kind`; parameters that do not fit
the model are left out (`warning`).

`GET /v1/apps/{id}/export.xlsx?component=c1&scope=<json>` — a component of an app as an
Excel file, computed as the user (`404` when they may not open the app) with the filters as
set (`scope`: `{params, bind}` as `/v1/eval` takes them). A spreadsheet: values typed (numbers,
dates, amounts with their currency in the number format), styles, merges, widths, heights,
frozen panes, sheet names; formulas as their values, their Quadra text in a note of the cell. A
chart: its data as a table (labels and series, at most its `max_points`). A KPI: its value, its
comparison (target, or the previous period) and its sparkline's points.

`PUT /v1/views/{id}/prefs {"filters": {"region": [12]} | null}` keeps (forgets) the user's own
default values of an app's filters: `my_filters` of the app as they read it.

### Folders

Apps are organized in folders (`kind: "folder"`), nested at most 10 deep, through the same
calls; `parent` is the folder an app or a folder is in (`null`: the top; saved views of
entities are in none).

| Call | What it does |
|---|---|
| `GET /v1/views?kind=app` | the apps, and `folders`: the folders shown to the user (below) — the tree in one answer |
| `GET /v1/views?kind=folder[&scope=all]` | the folders shown to the user, by name (`scope=all`, admins: every folder) |
| `POST /v1/views {kind: "folder", name, description?, parent?, shares?}` | a new folder, owned by the user: `201 {view}` |
| `POST /v1/views {kind: "app", …, parent?}` | a new app in a folder |
| `PUT /v1/views/{id} {base, parent}` | move an app or a folder (`null`: to the top) |
| `PUT /v1/views/{id} {base, name?, description?}` | rename a folder (a folder has no `config` nor `type`) |
| `PUT /v1/views/{id}/shares`, `/prefs`, `GET …/history` | as for apps |
| `DELETE /v1/views/{id}?base=N` | delete a folder (owner, admins): what it holds moves to its folder: `{deleted, version, moved, to}` |

A folder: a view with `kind: "folder"`, `type: "folder"`, `entity: null`, `config: {}`,
`parent`, and in lists `implied`. The rules, checked by the server:

- **Name**: 1 to 100 characters, a valid directory name (none of `/ \ : * ? " < > |` nor a
  control character, not `.` or `..`, not ending with `.`), unique among the folders with the
  same `parent` whatever the case and the owner (else `409`, “a folder named … exists already
  at the top”). Apps' names are not unique.
- **Place**: putting an app or a folder in a folder (creating it there or moving it) takes the
  right to change it (owner, co-owners, admins) and the right to change the folder: a folder
  shared to use, or shown only for what it holds, answers `403`; one the user does not see,
  `404`. A folder never goes into itself or a folder it holds (`400`), and nests at most 10
  deep counting what it holds (`400 "folders nest at most 10 deep"`).
- **Shown**: a user is shown the folders they own or that are shared with them, and every
  folder holding, at any depth, an app or a folder they are shown (`implied: true`: shown for
  what it holds only — it is not opened, nothing is put in it). Sharing a folder never shares
  what it holds.
- **Delete**: what the folder holds (apps and folders, whoever may see them) moves to the
  folder it was in, each a new version with the action `move: its folder was deleted`; refused
  (`409`) when a folder inside has the name of one already there.
- Moving is a change: a new `version`, in the history (`changed: ["parent"]`), `409` on a
  stale `base`, a `views` event. A change made on an older `base` is not a conflict when the changes
  since only moved it and it does not move it (an app moved while it was being edited: the
  edit is saved, in the folder it was moved to).

```http
POST /v1/views
{"kind": "folder", "name": "2026", "parent": 4}

201 {"view": {"id": 9, "kind": "folder", "type": "folder", "name": "2026", "parent": 4,
              "version": 1, "access": "owner", "can_edit": true, "can_share": true, …}}

PUT /v1/views/12
{"base": 3, "parent": 9}

200 {"view": {"id": 12, "kind": "app", "parent": 9, "version": 4, …}}

DELETE /v1/views/9?base=1

200 {"deleted": 9, "version": 2, "moved": 1, "to": 4}
```

`quadra db views export` writes folders with the apps and views (each with a `key`, its id
there, and its `parent`'s key; `"quadra_views": 2`); `import` finds each folder again by name
in its place, makes those missing, and puts every app back in its folder.

### Apps and views of interface files

An app or a saved view may come from an [interface file](/reference/interfaces) of the project
(`interfaces/**/*.qdi`): its JSON then has `"origin": "file"`, `"file":
"interfaces/sales/cockpit.qdi"` and, in production, `"read_only": true` (`can_edit: false`).
`PUT`, `DELETE` and a change of its shares with roles or everyone answer `409 {"error":
"defined in interfaces/sales/cockpit.qdi: change the file and deploy it, or duplicate it to
make an app of your own"}`; its shares with users and teams are changed as for any app. Under
`quadra dev` every change is accepted and written back to the file (a move to another folder
moves the file to the folder's directory: `409` when a file of that name is there).

`GET /v1/views/{id}/qdi` — any app or view as a file: `{"file": "sales-cockpit.qdi", "text",
"warnings", "defined_in"}` (warnings: what a file does not keep, such as shares with users).

### Custom forms

`POST /v1/apps/{id}/forms/{component}/submit` runs the statements of a custom form **of the
stored app** (never code sent by the client) as the caller, in one transaction, with exactly
their permissions:

```json
{ "values": { "number": "QD-0001", "customer": 12, "product": 3, "qty": 3 },
  "params": [{ "name": "region", "entity": "Order", "field": "customer.region", "widget": "multi", "value": [2] }],
  "idempotency_key": "f-4c1d…", "version": 7 }
```

`values` as `/v1/tx` takes them (an amount or a quantity also as a plain number in its unit);
`params` as `/v1/eval` takes them; `version`, optional, the version of the app the form was
opened on. Answers:

| Status | Body |
|---|---|
| `200` | `{tx, result: {entity, id, display} \| {value} \| null, message}` — `result`: the value of the last statement; `message`: the form's `message` |
| `200` | the same with `replayed: true`: the key was used for the same values, nothing written again |
| `422` | `{error, field?, fields?}`: a required field missing, a value of the wrong type, a `where` not met (at the field), a `require` or a `check` that fails, the form does not check |
| `403` | `{error}`: a write the caller may not make |
| `409` | `{error}`: the app changed since `version`, the key was used for other values, or others changed what the statements read (submit again) |

## The code of the project

`quadra serve … --editors admin,marco` lets those users edit the project from the browser
(the **Code** section of the web client). The project must be a git repository. Other users
get ``403 editing the code is not allowed for this user (see `quadra serve --editors`)``.

| Call | What it does |
|---|---|
| `GET /v1/dev/files` | the files of the working copy with their git status, and its folders (`dirs`) |
| `GET /v1/dev/file?path=` · `PUT /v1/dev/file {path, text}` · `DELETE /v1/dev/file?path=` | read, save (at most 1 MB), delete; `.qd`, `.csv` and `.md` files inside the project only (`.toml`, `.lock` and `.expected` can be read) |
| `POST /v1/dev/rename {from, to}` | moves a file or a folder; never onto something that exists (`409`) |
| `POST /v1/dev/dir {path}` · `DELETE /v1/dev/dir?path=` | creates a folder; deletes one with its content, only if every file in it could be deleted one by one (otherwise `409`). Hidden folders (`.git`) are never touched. |
| `POST /v1/dev/check {buffers}` | the diagnostics of the working copy with the unsaved `buffers` on top, with positions. A name declared in another module and not imported comes with `fix: {label, from, to, insert}`, the import to add. |
| `POST /v1/dev/schema {buffers}` | the data model of the working copy, for the data-model diagram: `entities` (kind, module, traits, fields with type, target entity or enum, `many`, `parent`, `unique`, `inherited`, location), `enums`, and `diff` against the running model: entities and fields `added`, `removed`, `changed`, `renamed` (by a `rename` the running model has not applied); a removed field with a new field of the same type in the same entity comes with `maybe_renamed_to`. While the code does not compile: the last version that did, with `stale: true`. |
| `POST /v1/dev/test {buffers}` | the project's tests |
| `GET /v1/dev/diff` · `GET /v1/dev/status` | the diff against `HEAD`; the branch, the history, the commit running |
| `POST /v1/dev/ide {op, path, offset, buffers}` | editor features at a position (UTF-16 offset): `definition`, `hover`, `complete`, `inspect` (the distribution of a field's values on the live data; inside a `check` or a formula, the expression evaluated on every record) |
| `POST /v1/dev/commit {message}` | a release, see below |

A commit is a release, step by step, stopping at the first failure:

1. **check**: no errors, certified metrics included;
2. **tests**: all pass;
3. **dry run** on the live data: pending migrations, new defaults, and the rules the new
   model adds or changes, on every record they apply to;
4. **commit** in git, with the user as author;
5. **apply**: the tables that changed are changed (columns added, renamed or dropped, new
   defaults written, formulas whose definition changed computed again) in one transaction,
   recorded in the signed log as `deploy <sha>: <message>`; on PostgreSQL the new indexes of
   existing tables are built right after, without blocking. If this fails, the git commit is
   reverted. See [What a deploy changes](/reference/migrations-and-changes#what-a-deploy-changes-in-the-database);
6. the server reloads the model; open clients receive a `deploy` event and reload theirs.
