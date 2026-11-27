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
| `history N unit` | an entity's line | the values replaced longer ago leave the history ([below](#a-shorter-history-history)) |

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

### A shorter history: `history`

```quadra
entity Sensor:
    history 1 year
    reading: Decimal
```

Once a day, the sweep takes out of the history the values replaced more than a year ago
(their digests stay). The value in force a year ago stays, and everything since. It suits
entities that change often and whose old values nobody needs: readings, levels, positions.
An entity that a frozen record reads cannot have it: a frozen record reads the world as it
was when it froze.

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
