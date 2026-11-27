# Step 16: Retention, erasure and a signed history

**Goal:** the shop keeps the data of people only as long as it should, and can prove its
history to an auditor:

- invoices are kept **ten years**, as the law asks (art. 2220 of the civil code);
- the requests of the web shop's contact form are kept **six months**, then erased by
  themselves;
- a customer who asks to be forgotten (GDPR, art. 17) loses their email and address,
  everywhere, unless invoices keep them;
- every commit is **signed** with the shop's key.

<SandboxLink project="tutorial-16" file="privacy.qd" label="Open this step in the Sandbox" />

::: warning Do this on your own machine
This step builds on step 15, whose PDFs and signatures run on your machine
([Installation](/guide/installation)). The tests of this step run in the Sandbox too.
:::

## How long: `retain` and `expire`

Invoices must be kept ten years after their date. One line in the entity says so:

```quadra{2}
entity Invoice:
    retain 10 years after date                # art. 2220 of the civil code
    number: Code? unique
    customer: Customer
    date: Date default today()
    # …
```

`retain` is a **minimum**: nothing may erase an invoice before then, nor the customer it
refers to.

The web shop's contact form collects names and emails that nobody needs after a while.
`privacy.qd`:

```quadra
# The data of people: what the web shop's contact form collects is kept six months; a customer
# who asks is forgotten (`Customer.forget`), unless invoices keep them.


entity Inquiry:
    expire 6 months after created_at
    name: Text? personal
    email: Email? personal
    message: Text?
```

`expire` is a **maximum**: six months after it was created, an inquiry is erased by itself.
The server's hourly sweep does it, in a transaction of its own:

```text
expire: Inquiry#77 (6 months after created_at)
```

The record leaves its table and every list; asked for by its id, the API answers `410`, with
when and why. Its history keeps only the digests of its values (more [below](#the-history-stays-verifiable)).

The time is counted from a `Date` or `DateTime` field, `created_at` or `updated_at`, in
`hours`, `days`, `weeks`, `months` or `years`. `retain` and `expire` also go on a single
field: `notes: Text? expire 30 days after created_at`.

## Forgetting a customer

Mark the fields that are data about the person as `personal`:

```quadra{4,6}
entity Customer:
    name: Text unique where n => n.len() >= 2
    kind: enum(Person, Business) default Person
    email: Email? unique personal
    vat_number: VatNumber? required when kind is Business
    address: Address? personal
    # …

    # GDPR art. 17: their email and address emptied, everywhere, unless invoices keep them
    action forget(reason: Text):
        erase self, reason
```

`erase` empties the `personal` fields of the record (and of its lines), and takes their past
values out of the history. The customer stays, with its name: its orders and invoices still
refer to it. An erased value is `none`, so a `personal` field is optional:

```text
error: `Customer.email` is personal: `erase` empties it, so it must be optional (`email: Email?`)
```

Who may erase is a permission of its own. In `security.qd`:

```quadra
# Who answers the requests of people about their data (GDPR art. 15, 17 and 20).
role Privacy is Staff:
    read Customer, Inquiry
    read history Customer
    run Customer.forget
    erase Customer
```

### What the law keeps

Anna has invoices. Asked to forget her, Quadra refuses, and says until when:

```bash
quadra erase Customer 38 --reason "she asked on the phone"
```

```text
error: erase: Customer#38 cannot be erased before 2036-11-27: Invoice#52 refers to it and is kept 10 years after date
```

That is what the GDPR itself says (art. 17(3)(b)): data the law requires you to keep is not
erased. Bea, who never ordered, is forgotten:

```text
✓ Customer#65 erased: Customer.email, Customer.address
  their past values are out of the history; their digests keep the chain valid
```

The tests say both cases:

```quadra
test "invoices keep their customer for ten years":
    use demo
    expect_error "cannot be erased before":
        anna.forget("she asked on the phone")
    expect anna.email == "anna@example.com"


test "a customer without invoices is forgotten":
    use shop
    let bea = Customer(name="Bea Verdi", email="bea@example.com")
    bea.forget("she asked by email")
    expect bea.email == none
    expect bea.address == none
    expect bea.name == "Bea Verdi"
```

And the inquiries, with `clock`, which runs the sweep as the server does:

```quadra
test "an inquiry is still there the day before":
    clock 2026-01-10 09:00
    Inquiry(name="Bea", email="bea@example.com", message="Do you ship to Austria?")
    clock 2026-07-09 09:00
    expect Inquiry.all().count() == 1
    clock 2026-07-10 09:01
    expect Inquiry.all().count() == 0
```

The checker guards what an erasure could break. An invoice kept ten years that showed
`customer.email` with a formula would change the day the email is erased:

```text
error: `Invoice.email` is kept 10 years after date, but `Customer.email` is personal: `erase` empties it: the formula would change when it does. Keep a copy with a `default` (taken when the record is created)
```

### A copy of what you hold

A person may also ask what you hold about them (art. 15 and 20):

```bash
quadra subject export Customer 38 > anna.json
```

```json
{
  "subject": { "entity": "Customer", "id": 38 },
  "personal_history": [
    { "at": "2026-11-27T23:10:21", "field": "email", "tx": 3, "value": "anna@example.com" },
    …
  ],
  "referred_by": [
    { "entity": "Customer", "id": 39, "field": "referred_by" },
    { "entity": "Invoice", "id": 52, "field": "customer" },
    { "entity": "Order", "id": 49, "field": "customer" },
    …
  ]
}
```

`GET /v1/records/Customer/38/export` gives the same. Erasing and exporting are both commits
of the history, with who asked and why.

## The history stays verifiable

Quadra keeps every value ever written, chained by hashes. How can a value be erased from it?
The chain covers the **digest** of each value, `sha256(salt | value)`, not the value: erasing
removes the value and its salt, and keeps the digest. The chain still verifies, and proves
that nothing else changed:

```text
✓ hash chain intact, tables match the history (56 records)
✓ 1 value erased, its digest kept
```

The erasure reaches the tables, the history, the log of the server (a snapshot follows at
once, and the older segments go), and the files of `File` fields that no value names any more.

## A signed history

The chain shows that the past was not altered. With a key, every commit is also signed, and
someone who computes the whole chain again without the key is found out:

```bash
quadra db key init
```

```text
✓ commit 2: the commits are signed by the key 535491efc05751560868022157377800cf0d6392f8e35f042eebf80b9a6e510c
  private key in /srv/shop/.quadra/signing.key (keep it out of the repository and of the database's backups)
```

```bash
quadra db verify
```

```text
✓ hash chain intact, tables match the history (56 records)
✓ 1 value erased, its digest kept
✓ 5 commits signed, the last ones by the key 535491efc057…
```

Keep the public key somewhere else, and give it to whoever checks a copy of your data:
`quadra db verify --key 535491…` refuses a chain that is not signed by it. On Quadra Cloud,
`quadra cloud env sign` makes the key for you.

## Years later

After a few years, most of the history is values replaced long ago. Two tools keep the
database small, without losing the proof:

- `history 1 year` on an entity whose old values nobody needs (stock levels, readings): the
  values replaced more than a year ago leave the history.
- `quadra db archive --before 2028-01-01` moves the values replaced before that date to an
  archive in the store of files; a signed checkpoint names it, and
  `quadra db verify --full` reads it back to check every commit.

## Run it

```bash
quadra test
```

```text
…
65 passed, 0 failed
```

## Try it

1. Make the phone number of a customer `personal`, and check that `forget` empties it.
2. Keep the web shop's abandoned carts two weeks: `expire 14 days after updated_at`.
3. Turn on signed commits on your server, then rotate the key: `quadra db key rotate`, and
   `quadra db verify`.

## Recap

- `retain` keeps data at least as long as the law asks; `expire` erases it when it is no
  longer needed, by itself.
- `personal` marks the data of a person; `erase` empties it everywhere, refused while a
  retention keeps it, with the date.
- `quadra subject export` gives a person a copy of what you hold about them.
- The history covers the digests of the values: an erasure keeps it verifiable.
- `quadra db key init` signs every commit; `quadra db verify` checks the signatures.

[Next steps →](./17-next-steps)
