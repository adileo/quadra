# Retention and erasure

Some data must be kept for years: invoices, ten years in Italy (art. 2220 of the civil
code). Some must not be kept longer than needed: the CV of a candidate who was not hired,
the sessions of a website. And a person may ask to be forgotten (GDPR, art. 17), unless the
law says the data must stay.

Quadra keeps the whole history of every value, chained by hashes ([Storage](/reference/storage#the-signed-commit-log)).
Retention and erasure live with it: the chain covers the **digests** of the values, not the
values. Erasing a value removes the value and keeps its digest, so the chain stays valid and
`quadra db verify` still proves that nothing else changed.

## `retain` and `expire`

```quadra
entity Invoice:
    retain 10 years after date              # the law's minimum: `erase` is refused before
    date: Date
    customer: Customer
    customer_name: Text? default customer.name

entity Candidate:
    expire 6 months after last_contact      # the maximum: erased then, by itself
    name: Text? personal
    email: Email? personal
    last_contact: Date
    cv: File<pdf, word>? expire 1 year after created_at

entity Session:
    expire 2 days after created_at
    token: Text

entity Contract:
    retain 10 years after end, then expire  # kept the minimum, then erased
    end: Date
```

| Form | Where | Means |
|---|---|---|
| `retain N unit after field` | an entity's line, or a field's modifier | kept at least that long: `erase` is refused before, with the date |
| `expire N unit after field` | an entity's line, or a field's modifier | erased at that moment, by itself |
| `retain …, then expire` | an entity's line | both, the same term |
| `personal` | a field's modifier | data about a person: `erase` empties it |
| `history N unit` | an entity's line | the values replaced longer ago leave the history ([below](#how-much-history-history)) |
| `history last N`, `history none`, `history full` | an entity's line, or `[data] history` for the project | the last N versions of each value, only the values in force, or all of them ([below](#how-much-history-history)) |
| `deletable`, `deletable, trash N unit` | an entity's line | its records can be deleted, through a trash first ([below](#deleting-records-deletable)) |

The unit is `hours`, `days`, `weeks`, `months` or `years` (`1 year` too). The time is counted
from a stored `Date` or `DateTime` field of the record, or from `created_at` or `updated_at`.
While that field has no value, the time does not run.

### What the checker says

A rule of retention that would break something is refused when the code is checked:

| Error | Why |
|---|---|
| `` `Candidate.name` is personal: `erase` empties it, so it must be optional (`name: Text?`) `` | an erased value is `none` |
| `` `Candidate.cv` expires: it is emptied then, so it must be optional `` | the same |
| `Contract expires (1 year after end) before the end of its retention (10 years after end)` | the law's minimum would not be kept |
| `` `Invoice.shown` is kept 10 years after date, but `Customer.name` is personal: `erase` empties it: the formula would change when it does. Keep a copy with a `default` `` | a document kept for years must not change when what it shows is erased: a `default` keeps a copy, taken when the record is created |
| `` `Sale.buyer` is personal: `erase` empties it: a ledger keeps its rows for ever, so Sales's rows cannot read it `` | ledgers are not erased |
| `Sale posts to Sales: its ledger rows are kept for ever, so its records cannot expire` | the same |
| `` `Visit.session` is required, and Session records expire (2 days after created_at): Visit must expire too, or the reference be optional `` | a required reference would be left empty |
| `` `fortnights` is not a unit of time here: hours, days, weeks, months or years `` | |
| `` `Candidate.name` is not a stored `Date` or `DateTime` `` | the time is counted from a date |

## When a retention ends

The server's periodic sweep (`quadra db sweep`, every hour with `quadra serve`) erases what
has come due, each record in a transaction of its own, written by the system with its cause:

```text
expire: Candidate#12 (6 months after last_contact)
```

- **A field that expires** becomes `none` (an empty list for a list). Its past values leave
  the history: their digests stay. The formulas that read it are computed again, in the same
  transaction.
- **A record whose entity expires** goes away, with its lines: its row leaves its table, it
  is no longer in `Candidate.all()`, in searches or in the lists of the web client, and the
  optional references to it become `none`. Its history keeps only digests. What is left is a
  line in `_quadra_erased`: its entity, when it was created and erased, and why.
  `GET /v1/records/Candidate/12` answers `410` with that:

  ```json
  {"error": "Candidate#12 was erased on 2026-07-11 09:00:00: expire: Candidate#12 (6 months after last_contact)"}
  ```

A record is not taken away while something keeps it: its own `retain`, the `retain` of a
record that refers to it, or a required reference from a record that does not expire yet. It
waits for the next sweep after that.

In tests, `clock` runs the same pass ([Testing](/reference/testing#clock)):

```quadra
test "a candidate expires six months after the last contact":
    clock 2026-01-10 09:00
    let k = Candidate(name="Bea", last_contact=today())
    clock 2026-07-11 09:00
    expect Candidate.all().count() == 0
```

### A deploy that erases at once

A new `expire`, or a shorter one, may make records that exist already expire at once. The
deploy says so before, and is refused:

```text
error: this version erases data at once:
  Candidate: 1204 records expire at once (6 months after last_contact)
Deploy it with --allow-expire if that is what you want
```

`quadra db migrate --allow-expire` deploys it, and the next sweep erases them. A term that
erases nothing at once comes into force by itself. Until a term is in force, the sweep does
not apply it.

## `erase`

```quadra
entity Customer:
    name: Text? personal
    email: Email? personal
    phone: Phone? personal
    vat_number: VatNumber?

    action forget(reason: Text):
        erase self, reason
```

`erase rec, "reason"` empties the `personal` fields of the record and of its lines, and takes
their past values out of the history (their digests stay). The record stays: the invoices
that refer to it still do. A field with a `retain` of its own that has not ended stays too.

It is refused while a retention keeps the record, with the date it ends:

```text
Customer#22 cannot be erased before 2036-11-27: Invoice#23 refers to it and is kept 10 years after date
```

(This is what the law says: art. 17(3)(b) of the GDPR.)

Who may erase is a permission of its own ([Security](/reference/security#rules)):

```quadra
role Privacy:
    read Customer
    read history Customer
    erase Customer
```

The same from outside the code:

| From | How |
|---|---|
| the API | `POST /v1/records/Customer/22/erase {"reason": "…"}`, or the `erase` operation of `POST /v1/tx` ([HTTP API](/reference/http-api#retention-and-erasure)) |
| the command line | `quadra erase Customer 22 --reason "she asked on 2026-11-27"` |

Every erasure is in the chain: the commit that made it, and a row of `_quadra_erasures` with
the record, the fields, the reason and who (`["e", …]` in the commit's hash).

### What erasure reaches

| Where | |
|---|---|
| the tables | the columns emptied, or the rows deleted |
| the history | the values and their salts removed, the digests kept |
| the log of the server (its segments and snapshots) | a snapshot follows at once, and the segments before it are dropped |
| a copy of an earlier moment rebuilt from the log | the erasures that came after are applied to it |
| the archives of the history | the values taken out, within the hour |
| the files of `File` fields | removed from the store once no value names them, within the hour |
| backups made outside Quadra | they keep what they copied until they are themselves deleted: keep them for a bounded time (30 days is common), and a backup restored gets the erasures again from the log |

## Exporting a person's data

Art. 15 and 20 of the GDPR give a person the right to a copy of their data:

```bash
quadra subject export Customer 22 > anna.json
```

or `GET /v1/records/Customer/22/export`. The answer has the record and its lines as the API
shows them, the history of its `personal` fields (when, by whom, the value), and the records
that refer to it:

```json
{
  "subject": {"entity": "Customer", "id": 22},
  "record": {"…": "…"},
  "lines": [],
  "personal_history": [{"tx": 3, "at": "2026-01-10T09:00:00", "by": "clara", "field": "email", "value": "anna@example.com"}],
  "referred_by": [{"entity": "Invoice", "id": 23, "field": "customer"}]
}
```

Reading it needs `read` and `read history` of the entity. The export is a commit of the
chain (`export: Customer#22`): who exported whom, and when.

## The digests in the chain

Each value of the history has a salt, 16 random bytes. The hash of a commit covers, for each
value, `sha256(salt | value)`:

```text
hash = sha256(prev_hash | tx | at | user | cause | ["h", record, field, digest] … )
```

An erased value loses its value and its salt; its digest stays in `_quadra_history.digest`.
`quadra db verify` computes the digests of the values that are there and uses those kept for
the others:

```text
✓ hash chain intact, tables match the history (26 records)
✓ 3 values erased, their digests kept
```

Without the salt, the digest of an erased name would let anyone check a guess (`Mario
Rossi`). The salt goes with the value. `quadra test` derives the salts from the position of
each value instead, so that the same tests give the same chain on every run.

## Signed commits

The chain shows that the past was not altered, but whoever controls the database could
compute it all again. With a signing key, every commit also carries an Ed25519 signature of
its hash: without the key, a chain computed again has no valid signatures.

```bash
quadra db key init
```

```text
✓ commit 2: the commits are signed by the key ade3018dc1865b17a7287e21ca84f39537bc287d9255e8ada5980b23e0078d73
  private key in /srv/shop/.quadra/signing.key (keep it out of the repository and of the database's backups)
```

The private key is never in the database. It is read from `QUADRA_SIGNING_KEY` (the key as
text, `ed25519:` and 64 hex digits), or the file `QUADRA_SIGNING_KEY_FILE`, or
`.quadra/signing.key` of the project. A process that does not hold the key of a database
that signs its commits cannot commit to it:

```text
this database signs its commits with the key ade3018dc186…, which this process does not hold: set QUADRA_SIGNING_KEY (or QUADRA_SIGNING_KEY_FILE) to its private key
```

| Command | |
|---|---|
| `quadra db key` | the public key, and where the private key is |
| `quadra db key init` | the first key |
| `quadra db key rotate` | a new key: a commit records it, signed by the old one; the old commits stay verifiable |
| `quadra db verify --key <public>` | checks that the chain ends with that key |

`quadra db verify` checks every signature, and that the chain is signed by the key this
process holds (or `--key`):

```text
✗ commit 12: signature does not match the key
✗ the chain is not signed: the expected key is ade3018dc186…
```

The second is what someone who removed the signatures and computed the chain again would
leave: keep the public key somewhere else, and give it to whoever checks a copy of the data.

On Quadra Cloud, `quadra cloud env sign` makes the key of an environment: the platform keeps
it, sealed like the other secret variables (`QUADRA_SIGNING_KEY`), and prints its public
key. A preview or a fork that turns it on gets a key of its own.

## Databases that grow

The tables hold the values in force; what grows for years is the history of every value
replaced.

### How much history: `history`

The history keeps every version of every value, by default. An entity says how much of it
it needs:

```quadra
entity Invoice:                 # (nothing said: every version, for ever)
    number: Code

entity Sensor:
    history 1 year              # the versions replaced more than a year ago go
    reading: Decimal

entity Page:
    history last 10             # the last 10 versions of each value
    body: Text

entity Session:
    history none                # only the values in force
    token: Text
```

| | keeps | the chain covers | for |
|---|---|---|---|
| `history full` (the default) | every version, for ever | every value | what must be audited: invoices, payments, stock, users |
| `history 1 year` | every version until a year after it was replaced | every value (the digests of those gone) | readings, levels, positions |
| `history last 10` | the last 10 versions of each value | every value (the digests of those gone) | the pages of a site, documents edited often |
| `history none` | the value in force of each field, nothing else | nothing of it: its commits, not its values | sessions, carts, caches, counters |

Once a day the sweep takes the versions beyond what the entity keeps out of the history
(their digests stay, the chain still verifies). An entity with `history none` writes no
version: the history holds only a copy of each value in force, outside the chain, that a
write replaces; its record deleted, nothing of it is left. What it loses:

- "as it was then": the history tab of the studio shows what it keeps, or says it keeps none;
- the frozen records that read it: an entity frozen records read keeps its history whole, and
  the checker refuses `history last` or `none` on it (they read the world as it was when they
  froze);
- `quadra db verify` checks its commits, not its values.

A project of a website, where most data is not audited, says it once:

```toml
[data]
history = "last 10"         # the default of the project's entities: full, none, "last 10", "1 year"
```

An entity that says `history …` keeps what it says (`history full` for the users, the
orders); the entities of the standard library and those frozen records read keep their whole
history.

### Archives: `quadra db archive`

```bash
quadra db archive --before 2028-01-01
```

```text
✓ commit 4512: 1820431 values replaced before 2028-01-01 moved to archive/history-00000000000000004498-6be431e30f2b.jsonl.gz
  sha256 6be431e30f2b…
  `quadra db verify --full` reads it back and checks every commit
```

The values replaced before the date move to an archive in the project's store of files (its
folder, or its bucket: [Files](/reference/types#files)), compressed, and leave the database.
A checkpoint commit names the archive and its SHA-256, computed over the digests of its rows,
and is signed like any commit. The values in force stay, and every value an entity read by
frozen records had.

| | |
|---|---|
| `quadra db verify` | checks the commits after the checkpoint as usual, and those before it by their links and signatures |
| `quadra db verify --full` | reads the archives back (their rows must be the checkpoint's) and computes every commit |
| the history of a record | `GET /v1/records/{E}/{id}/history` reads the archived values back with its last page, marked `"archived": true` |

A value erased after it was archived leaves the archive too: the archive is written again
without it, and is still the one its checkpoint names.

The archive takes, with the values replaced, every value left of the records gone by then
(expired, deleted for good: their digests), so what is left of them does not grow for ever.

The sweep does it by itself, once a day, when the project says how old:

```toml
[data]
archive_after = "2 years"
```

The checkpoint is a commit like the others, through the server's sequencer and its log: on
Quadra Cloud it runs this way, in the tenant's bucket.

## Deleting records: `deletable`

Records are not deleted unless their entity says so: an invoice is cancelled or credited, and
stays. The records of an entity that says `deletable` can be:

```quadra
entity Page:
    deletable, trash 30 days        # to the trash first, for 30 days
    history last 10
    slug: Text unique
    title: Text
    blocks: [Block]

entity Block:
    page: Page parent
    text: Text

entity Session:
    deletable                       # deleted for good at once
    history none
    token: Text

role Editor:
    read, create, edit, delete Page
```

| | |
|---|---|
| in code | `delete page`, `restore page` |
| the API | `DELETE /v1/records/Page/12`, `POST /v1/records/Page/12/restore`, `GET /v1/trash`, or the operations `delete` and `restore` of `POST /v1/tx` ([HTTP API](/reference/http-api#deleting-records)) |
| the studio | **Delete** in the record's panel; the **Trash**, with **Restore** |
| who | the permission `delete Page` (it restores too) |

A record deleted goes with its lines. The optional references to it become none; a required
reference holds it, and so does a retention:

```text
Page#12 cannot be deleted: Comment#40 refers to it (`Comment.page` is required)
Invoice#23 cannot be deleted before 2036-03-01: Invoice is kept 10 years after date
```

`on Page deleted as p:` runs first, while its values are there.

**The trash.** With `trash 30 days`, a record deleted leaves every table, list and search,
and its unique values are free (a new page may take its slug); it is kept whole, with its
lines and the references it emptied. `restore` gives it back with its id, and puts those
references back where they are still empty; it is refused when a value of it that is unique
was taken meanwhile. Asked for by its id, the API answers `410` with until when. When its time
is over, the sweep deletes it for good. In tests, `clock` does the same.

**What is left of it** is what its entity's history keeps: everything with `history full` (it
is the audit of what the record was, readable as of any earlier moment), nothing with `history
last N` or `none` (the values go when it is deleted for good). A record of `history none`
deleted for good leaves nothing but its commit: who deleted what, and when. The space it
took goes back to the file system within the day ([below](#space-on-disk)).

## Messages and events

The messages an `inbound` received and the events an `outbound` sent are kept for a term once
they are closed: 90 days and 30 days, or what the project says (`[data] inbox`, `outbox`), or
what an `inbound` or an `outbound` says (`keep 10 years`, `keep forever`, `keep 30 days, then
archive`). What is pending, failed or rejected stays ([Connections](/reference/connections#outbound)).

## Space on disk

What is erased, expired or deleted is overwritten with zeros in the database's file
(`secure_delete`): an erased value leaves no trace in its free pages, nor in the copy a
server uploads when it goes to sleep. After an erasure the server writes its WAL back into the
file and empties it, at once.

The pages freed go back to the file system once a day, and at once after an erasure (SQLite's
incremental `auto_vacuum`): a database that deleted half of its records is half as large the
next day. A database made by an earlier version is turned into one that does this the first
time (a `VACUUM`, once).

## Hot and cold: `archive`

A database that grows for years keeps in use only what is needed: the records that are
closed go cold, to volumes beside the database, and are read on demand, more slowly. A cold
record is a slow record, not another one: `Invoice.all()`, queries, totals, searches,
references and `unique` still find it, and writing to it brings it back.

```quadra
entity Invoice:
    archive 1 year after date
    …

entity Customer:
    archive never                   # stays in use: it is read all the time
```

```toml
[data]
cold_after = "3 years"              # the project's entities: 3 years after their last write
```

A record is **closed**, and goes cold, when:

- its term is over (`1 year after date`), and it was not written for as long;
- no transition of its flows can take it out of its state now: an order still `Open` stays in
  use, however old; an invoice `Posted` whose `cancel` says `unless paid > 0 EUR` is closed
  once it is paid;
- no record that stays in use refers to it: an open order keeps its customer and its
  products in use (they would be read with it). A cold record may refer to one in use;
- its lines go with it (a line has no `archive` of its own).

The checker refuses what could never close (every state of a flow has an unguarded way out),
an `archive` on a line or a ledger, and one that would come after the record's `expire`.

### Volumes

Once a day the server moves the records closed since the last time to a new **volume**: a
SQLite file with their rows (of their tables, their history, what they post), beside the
database (`data.db.cold/`) and in the project's store of files (`cold/<volume>.db`, the
tenant's bucket on Quadra Cloud). A volume never changes; a commit names it with its SHA-256,
so `quadra db verify` checks the cold records with the others. With many volumes, the next one
holds the records of the older ones too.

| | |
|---|---|
| `quadra db cold [--at 2029-01-01]` | how many records are closed now (or on that day), about how much they hold, and why the others are not |
| `quadra db cold --run` | moves them now |
| `quadra db cold --fetch` | brings the volumes not beside the database from the store of files (a database rebuilt from its log, another machine); the server does it before it reads |

**Writing** to a cold record (an action, an update, `erase`, `delete`, a retention that ends)
brings it back into the database in use first, in the same commit. **A deploy** brings back
the cold records of the entities whose fields, formulas or defaults it changes (every one, for
a deploy that rebuilds every table); the others stay cold.

A database that uses volumes has no foreign keys into the entities that may go cold: a
record in use may refer to a cold one, and the engine keeps the references.

### The same results: `quadra test --cold`

```bash
quadra test --db sqlite --cold
```

Before each statement of each test, every record that is closed goes cold, whatever its term
(its state and what refers to it still count): the next statements read and write through the
volumes. The tests must give the same results: a cold record is a slow record, not another
one.

### What it costs

The editor says when a query may read the cold records: `Order.where(o => o.total > 0)` reads
every order, old ones too; a condition on the term's field (`o.date >= today() - 365 days`)
keeps it to the recent ones, and a total kept by a formula (`sum` over a record's lines) does
not read them at all.

A value erased from a cold record leaves the database in use at once, and its volume within
the day: a volume holding records that came back (written, erased, deleted) is written again
without them by the next run, under a new name and a commit of its own, and the old file goes,
beside the database and from the store of files.

::: warning Not yet
Volumes are fetched whole, not page by page.
:::
