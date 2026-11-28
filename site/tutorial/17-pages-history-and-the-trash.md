# Step 17: Pages, history and the trash

**Goal:** the web shop gets a site of its own, written by an editor. Its pages are edited
often and sometimes deleted; nobody needs to audit them, and they must not fill the disk:

- a page keeps its **last ten versions**, not every one;
- a page deleted goes to the **trash** for thirty days, and comes back with a click;
- a visitor's basket keeps **only what it holds now**, and is gone two days later.

<SandboxLink project="tutorial-17" file="site.qd" label="Open this step in the Sandbox" />

## How much history: `history`

Until now every entity kept every version of every value: an invoice, an order, a payment
must be audited, and the history is what proves what happened. A page of the site does not:
ten versions are enough to go back after a mistake. `site.qd`:

```quadra
entity Page:
    deletable, trash 30 days
    history last 10
    slug: Text unique where s => s.len() >= 2
    title: Text
    body: Text?
    sections: [Section]


entity Section:
    page: Page parent
    heading: Text
    text: Text?
```

Once a day the server's sweep takes the versions beyond the last ten out of the history
(their digests stay, so the chain of commits still verifies). Correcting a comma in a long
`body` stores the whole new text: `last 10` keeps that from growing for ever.

A basket keeps nothing of its past at all:

```quadra
entity Basket:
    deletable
    history none
    expire 2 days after created_at
    product: Product?
    qty: Int default 1
```

| | keeps | for |
|---|---|---|
| nothing said (`history full`) | every version, for ever | invoices, orders, payments, users |
| `history 1 year` | the versions replaced in the last year | readings, levels |
| `history last 10` | the last ten versions of each value | pages, documents edited often |
| `history none` | only the values in force | baskets, sessions, caches |

The checker refuses less history where it would change what a record shows: an invoice frozen
when it was posted reads its customer as it was then, so `Customer` keeps its whole history.

A project where most data is not audited says it once, in `quadra.toml`, and the entities that
need their history say `history full`:

```toml
[data]
history = "last 10"
```

## Deleting a page: `deletable`

Records in Quadra are not deleted unless their entity says so: an invoice is credited, an
order is cancelled, and both stay. A page may go. `deletable, trash 30 days` sends it to the
trash first; `deletable` alone deletes for good, as for the baskets.

Who may delete is a permission of its own. In `security.qd`:

```quadra
role Editor is Staff:
    read, create, edit, delete Page
    read, create, edit Section              # deleted with their page
    read Removed
```

In the studio the record's panel has **Delete**; the **Trash** lists what was deleted, with
**Restore**. From code, `delete p` and `restore p`; from the API, `DELETE
/v1/records/Page/12` and `POST /v1/records/Page/12/restore`.

A page in the trash is out of every list and search, its sections with it, and its slug is
free for a new page. Restored, it comes back with its id and its sections:

```quadra
test "a page goes to the trash and comes back":
    use shop
    let p = Page(slug="shipping", title="Shipping")
    Section(page=p, heading="Italy", text="Two days")
    delete p
    expect Page.all().count() == 0
    expect Section.all().count() == 0
    restore p
    expect p.title == "Shipping"
    expect p.sections.count() == 1
```

Its slug taken meanwhile, it cannot come back:

```quadra
test "the slug of a page in the trash is free":
    use shop
    let old = Page(slug="about", title="About us")
    delete old
    Page(slug="about", title="Who we are")
    expect_error "unique":
        restore old
```

Thirty days later the sweep deletes it for good. With `history last 10`, nothing of its values
is left; with the whole history it would stay, readable as of any earlier moment, as the audit
of what it was:

```quadra
test "thirty days later the trash is empty":
    clock 2026-03-01 09:00
    let p = Page(slug="sale", title="Winter sale")
    delete p
    clock 2026-04-01 09:00
    expect_error "is not in the trash":
        restore p
```

### What holds a record

A record that something needs is not deleted, and the error says why:

```text
Page#12 cannot be deleted: Link#40 refers to it (`Link.page` is required)
Invoice#23 cannot be deleted before 2036-03-01: Invoice is kept 10 years after date
```

An optional reference to it becomes `none`, and comes back with a restore.

### Before it goes: `on Page deleted`

```quadra
entity Removed:
    slug: Text
    title: Text

on Page deleted as p:
    Removed(slug=p.slug, title=p.title)
```

The reaction runs in the same transaction, before the page goes: its values are still there.

## The space on disk

What is deleted, expired or erased gives its space back: the database's file is overwritten
with zeros where the values were, and the pages freed return to the file system once a day.
The messages received from the web shop and the events sent to it are kept 90 and 30 days
once handled, then pruned; another term is one line:

```toml
[data]
inbox = "1 year"
outbox = "30 days, then archive"     # to an archive in the store of files first
archive_after = "2 years"            # the history older than that, moved to archives
```

## Run it

```bash
quadra test
```

```text
…
69 passed, 0 failed
```

## Try it

1. Give `Section` a trash of its own, and delete a section alone.
2. Keep the inquiries of `privacy.qd` with `history none`: what does the checker say?
3. Set `[data] history = "last 10"` and see which entities keep their whole history anyway.

## Recap

- `history full`, `1 year`, `last 10`, `none`: each entity keeps the history it needs; the
  project's `[data] history` is the default.
- `deletable` lets an entity's records be deleted, with the permission `delete`;
  `trash 30 days` keeps them restorable first.
- A required reference or a retention holds a record; `on … deleted` runs before it goes.
- What goes gives its space back, and the messages and events are kept for a term.

[Next steps →](./18-next-steps)
