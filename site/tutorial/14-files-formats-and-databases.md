# Step 14: Files, formats and databases

<SandboxLink project="tutorial-14" file="counter.qd" />

**Goal:** not every system speaks HTTP. In the shop:

- the **till** at the counter writes its sales of the day to a CSV file, on an SFTP server in
  the shop's network, and reads its prices from fixed-width files;
- the **bank** puts its statements on an FTP server, as fixed-width records: each credit
  that names an invoice should become its payment;
- the **accountant's software** wants our posted invoices in a table of its own PostgreSQL
  database, in the back office.

The till and the accountant's database are behind the shop's firewall, where Quadra cannot
reach them. An **on-premise agent** runs there and does the work for it.

## The connections

```toml
[vars]
TILL_SFTP_KEY = { secret = true, description = "the private key the till's SFTP server knows us by" }
BANK_FTP_PASSWORD = { secret = true }
ACCOUNTING_DB_PASSWORD = { secret = true }

# The till at the counter: an SFTP server in the shop's network, reached by our agent there.
[connections.till]
kind = "sftp"
url = "sftp://till.local/data"
user = "quadra"
private_key = { var = "TILL_SFTP_KEY" }
known_host = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl"
location = "agent:backoffice"

# The bank: its statements, on FTP over TLS.
[connections.bank]
kind = "ftp"
url = "ftps://ftp.bank.example.com/statements"
user = "beans-and-leaves"
password = { var = "BANK_FTP_PASSWORD" }

# The accountant's software: a PostgreSQL database in the back office, reached by the agent.
[connections.accounting]
kind = "postgres"
url = "postgres://accounts.local:5432/books"
user = "quadra"
password = { var = "ACCOUNTING_DB_PASSWORD" }
location = "agent:backoffice"
insecure = true                             # no TLS inside the back office
```

- `known_host` is the SFTP server's public key (`ssh-keyscan -t ed25519 till.local`). A
  server showing another key is refused, as someone posing as it.
- `location = "agent:backoffice"` sends the connection's work to the agent named
  `backoffice`. The agent holds the till's key and the database's password, so the app never
  needs them.

## The till's closing: CSV in

The till writes this, every evening, in `closings/2026-11-27.csv`:

```text
CASSA;DATA;CODICE;QTA;IMPORTO
T1;27/11/2026;MUG-01;2;15,00
T1;27/11/2026;BEANS-01;0,5;9,00
```

The till is Italian: semicolons, decimal commas, dates day first. A record `from csv` says so
once, and names each column:

```quadra
record TillRow from csv(sep=";", decimal=","):
    till: Code at "CASSA"
    day: Date at "DATA" format "%d/%m/%Y"
    sku: Code at "CODICE"
    qty: Decimal at "QTA"
    amount: Decimal at "IMPORTO" decimals 2
```

Each row becomes a `CounterSale`, which takes the goods out of the stock ledger of step 10:

```quadra
entity CounterSale:
    day: Date
    till: Code
    product: Product
    qty: Qty<product.uom> where > 0
    amount: Money<EUR>

    post to Stock:
        add(product=product, qty=-qty)


# Every evening the till drops `closings/<day>.csv`; we look for new files every 15 minutes.
inbound TillClosing via till as r: TillRow:
    pull "closings/*.csv" every 15 minutes
    atomic per file                         # a closing is one thing: all of it, or none
    let p = Product.where(x => x.sku == r.sku).first() ?? fail f"unknown product {r.sku}"
    CounterSale(day=r.day, till=r.till, product=p, qty=Qty(r.qty, p.uom),
        amount=(1.00 EUR * r.amount).round())
```

- `pull "closings/*.csv" every 15 minutes` looks in the folder for new files. A file still
  being written is left for the next look.
- Each file is a **message**, as in step 13: kept, never handled twice.
- The code runs **once per row**. `r` is a row.
- `atomic per file`: all the rows in one transaction. One bad row (an unknown product, more
  than we have in stock) rejects the whole file, and nothing is written.
- Once handled, the file is moved to `closings/processed/`, or to `closings/rejected/` with a
  `.report.txt` beside it that says which line was wrong and why.

## The bank's statement: fixed-width in

Banks and older systems write **fixed-width** records: each field in its own columns, with no
separators. Numbers have no decimal point (`0000000008792` is 87.92), and dates are `ddmmyy`:

```text
622711260000000008792CINV/2026/00001 thank you
622711260000000001200Dbank fees
```

```quadra
record BankMovement from fixed:
    kind: Code at "1..2"                    # 62: a movement; other kinds are headers and totals
    value_date: Date at "3..8" format "%d%m%y"
    amount: Decimal at "9..21" decimals 2   # 0000000008792 is 87.92
    sign: Code at "22"                      # C credit, D debit
    reference: Text at "23..72"             # what the customer wrote: "INV/2026/00001 thanks"


inbound BankStatement via bank as m: BankMovement:
    pull "*.txt" every 1 hours
    atomic per row                          # each movement stands alone
    if m.kind == "62" and m.sign == "C":
        let invoice = Invoice.where(i => i.state is Posted and m.reference.contains(i.number ?? "?"))
            .first() ?? fail f"no posted invoice named in `{m.reference}`"
        Payment(invoice=invoice, date=m.value_date, amount=(1.00 EUR * m.amount).round(),
            method=Transfer)
```

A statement is the opposite of a closing. Each movement stands alone, so `atomic per row`:
a credit that names no invoice is refused and listed in the report, and the others go in.
Each `Payment` posts to the receivables ledger of step 10, so the customer's balance follows.

::: tip AS/400 and mainframes
The same records read EBCDIC files byte for byte (`from fixed(encoding="ibm-1144")`), with
`packed` and `zoned` numbers: see [Fixed-width](/reference/connections#fixed-width).
:::

## Prices for the till: files out

The till reads one fixed-width file per product. `publish` from step 13 also works with a
file connection. The block ends with `write` instead of an HTTP request:

```quadra
record TillPrice from fixed:
    sku: Code at "1..12"
    name: Text at "13..42"
    price: Decimal at "43..51" decimals 2
    per_kg: Bool at "52" format "K/P"

publish Product to till as p:
    when p.active
    on price, name
    write(f"prices/{p.sku}.txt", [TillPrice(sku=p.sku, name=p.name,
        price=p.price * Qty(1, p.uom) / 1 EUR, per_kg=p.uom == kg)])
```

The file is written under a hidden name and renamed when complete, so the till never reads
half a file.

## The accountant's database: rows out

The accountant's software has agreed on an **exchange table**. We insert a row there, and their
software takes it in with its own checks. We never write into its own tables:

```sql
CREATE TABLE exchange.invoices (
    "DOC_NUMBER" VARCHAR(20) NOT NULL,
    "DOC_DATE" DATE NOT NULL,
    "CUSTOMER" VARCHAR(100) NOT NULL,
    "VAT_NUMBER" VARCHAR(20),
    "TAXABLE" NUMERIC(14, 2) NOT NULL,
    "VAT" NUMERIC(14, 2) NOT NULL,
    "TOTAL" NUMERIC(14, 2) NOT NULL,
    quadra_key VARCHAR(100) UNIQUE,        -- each invoice once, even if sent again
    taken_at TIMESTAMP                     -- set by their software when it takes the row
);
```

```quadra
event InvoicePosted(invoice: Invoice, number: Text)

on Invoice enters Posted as i:
    emit InvoicePosted(invoice=i, number=i.number ?? "")


# A row of the exchange table, its columns as the accountant's database names them.
record AccountingInvoice from json:
    number: Text at "DOC_NUMBER"
    date: Date at "DOC_DATE"
    customer: Text at "CUSTOMER"
    vat_number: Text? at "VAT_NUMBER"
    taxable: Decimal at "TAXABLE"
    vat: Decimal at "VAT"
    total: Decimal at "TOTAL"


outbound InvoicePosted via accounting as e:
    ordered by e.invoice.customer           # one customer's invoices in the order posted
    let i = e.invoice
    insert("exchange.invoices", AccountingInvoice(number=e.number, date=i.date,
        customer=i.customer.name, vat_number=i.customer.vat_number,
        taxable=(i.subtotal + i.shipping) / 1 EUR, vat=i.vat / 1 EUR, total=i.total / 1 EUR))
```

`insert` writes one row, its values bound as parameters. With a `quadra_key` column, an
event sent again is not written twice. Reading works the other way round:
`inbound … via accounting` with `pull "TABLE"` and a `cursor` reads a table a page at a time,
from where it stopped ([Databases](/reference/connections#databases-of-other-systems)).

## Formats are values

Reading and writing a format are plain functions, which makes them easy to test:

| Format | Read | Write |
|---|---|---|
| JSON | `WebOrder.parse(text)` | `order.json()` |
| XML | `Invoice.parse(text)` | `invoice.xml()` |
| CSV | `TillRow.parse_rows(text)` | `rows.csv()` |
| Fixed-width | `BankMovement.parse_rows(text)` | `rows.fixed()` |

```quadra
const CLOSING = """CASSA;DATA;CODICE;QTA;IMPORTO
T1;27/11/2026;MUG-01;2;15,00
T1;27/11/2026;BEANS-01;0,5;9,00
"""


test "the till's file is read as rows":
    let rows = TillRow.parse_rows(CLOSING)
    expect rows.count() == 2
    expect rows[1].qty == 0.5
    expect rows.sum(r => r.amount) == 24.00
    expect rows.csv().starts_with("CASSA;DATA;CODICE;QTA;IMPORTO\r\nT1;27/11/2026;MUG-01;2;15,00\r\n")
    expect TillRow.parse_rows(rows.csv()) == rows       # written and read back: the same rows


test "a fixed-width record, column by column":
    let p = TillPrice(sku="BEANS-01", name="House blend", price=19.00, per_kg=true)
    expect [p].fixed() == "BEANS-01    House blend                   000001900K\r\n"
```

A file that does not fit says where:

```text
TillRow: line 3, column `QTA`: a number with `,` before the decimals was expected, not `1.5`
```

## Tests

`receive` takes the text of a file too. For a CSV or fixed-width record, it handles the rows as
the inbound's `atomic` line says:

```quadra
test "a closing becomes counter sales and leaves the stock":
    use shop
    receive TillClosing CLOSING
    expect CounterSale.count() == 2
    expect mug.on_hand == 48 pcs
    expect beans.on_hand == 19.5 kg


test "one bad row rejects the whole closing":
    use shop
    expect_error "unknown product DECAF-01":
        receive TillClosing """CASSA;DATA;CODICE;QTA;IMPORTO
T1;27/11/2026;MUG-01;2;15,00
T1;27/11/2026;DECAF-01;1;9,00
"""
    expect CounterSale.count() == 0
    expect mug.on_hand == 50 pcs


test "a bank credit pays the invoice it names":
    use demo
    let inv = cafe.orders.flat_map(o => o.invoices).first() ?? fail "no invoice"
    let number = inv.number ?? ""
    let statement = [
        BankMovement(kind="61", value_date=2026-11-27, amount=0, sign="C", reference="opening"),
        BankMovement(kind="62", value_date=2026-11-27, amount=87.92, sign="C",
            reference=f"{number} thank you"),
        BankMovement(kind="62", value_date=2026-11-27, amount=12.00, sign="D", reference="bank fees"),
    ]
    receive BankStatement statement.fixed()
    expect inv.payment_status is Paid
    expect cafe.balance == 0 EUR


test "a posted invoice goes to the accountant's database":
    use shop_with_orders
    let inv = order.create_invoice()
    inv.post()
    expect sent InvoicePosted(invoice=inv)
```

The statement in the bank test is built with `BankMovement(…)` and written with `.fixed()`:
no column counting by hand.

```bash
quadra test
```

```text
…
55 passed, 0 failed
```

## The agent

The till and the database are in the back office. On a machine there (a small server, a
spare PC), the agent opens connections **only outwards**, to the app, and asks it for work.
No port is opened in the firewall.

Make a one-time code for it. In the studio this is **Integrations → Agents → Add an agent**;
with your own server, use the CLI:

```bash
quadra agent add backoffice
```

```text
✓ agent `backoffice`: its one-time code is 22MY-2UB6-PS25 (valid an hour)
  in the network it reaches: quadra agent run --server <the app's address> --code 22MY-2UB6-PS25
  and on its connections: location = "agent:backoffice"
```

On the back office machine, put the secrets of its connections in a `.env` file. They stay
there:

```text
ACCOUNTING_DB_PASSWORD=…
TILL_SFTP_KEY=…
```

Then start the agent with the code:

```bash
quadra agent run --server http://127.0.0.1:8080 --code 22MY-2UB6-PS25
```

```text
✓ registered as `backoffice`: the key is in quadra-agent.toml (keep it private)
quadra agent 0.10.0 `backoffice` for http://127.0.0.1:8080: waiting for jobs (Ctrl-C to stop)
2026-11-27 19:41:17  accounting: insert into exchange.invoices: done
2026-11-27 19:41:17  accounting: insert into exchange.invoices: done
```

The two invoices of the demo data are now in the accountant's table:

```text
   DOC_NUMBER   |  DOC_DATE  |    CUSTOMER    |  VAT_NUMBER   | TAXABLE |  VAT  | TOTAL  |        quadra_key
----------------+------------+----------------+---------------+---------+-------+--------+---------------------------
 INV/2026/00001 | 2026-11-27 | Caffè Centrale | IT01234567890 |  162.00 | 25.92 | 187.92 | quadra-3a2fb8e8bdbd6920-1
 INV/2026/00002 | 2026-11-27 | Anna Rossi     |               |   40.90 |  5.40 |  46.30 | quadra-3a2fb8e8bdbd6920-2
```

The code is used once. The agent keeps a key of its own in `quadra-agent.toml`, and
afterwards `quadra agent run` in that folder is enough. Run it as a service of the system,
so it starts with the machine. A lost or stolen machine is revoked in seconds:

```bash
quadra agent revoke backoffice
```

```text
✓ `backoffice` revoked: it receives nothing more within 5 s
```

and the agent stops:

```text
error: this agent was revoked, or its key is not valid here: register it again (a new code)
```

## Try it

1. Read the supplier's restocks from their database: `inbound RestockIn via suppliers_db`,
   `pull "public.deliveries" every 10 minutes` with `cursor m.delivered_at`, creating a
   `Restock` that is received at once.
2. Make the closing `atomic per row` and write a test where one row of three is refused:
   what is in the stock afterwards?
3. Export the day's counter sales to the accountant as a CSV file on the till's server with
   `write(f"exports/{day}.csv", rows)`.

## Recap

- `sftp` and `ftp` connections **pull** files from a folder (`pull "folder/*.csv" every …`),
  move them to `processed/` or `rejected/` with a report, and **write** files atomically.
- Records `from csv` and `from fixed` (and `json`, `xml`) describe a format once. Reading and
  writing them are plain functions you can test.
- `atomic per file` when a file is one thing, `atomic per row` when each row stands alone.
- `postgres`, `mysql`, `sqlserver` and `odbc` connections read a table from a cursor and
  `insert` into exchange tables, never into another system's own tables.
- `location = "agent:<name>"` runs a connection through an on-premise agent. It connects
  only outwards, keeps the credentials, and is revoked in seconds.

[Next: documents, labels and signatures →](./15-documents-labels-and-signatures)
