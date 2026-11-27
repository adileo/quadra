# Documents, labels and signatures

An app produces documents: the invoices it sends, the delivery notes, the labels on the
parcels, files signed for the tax authority or encrypted for the bank. In Quadra they are
written in **`.qdp` files** (Quadra print) beside the code, with the same lexer, comments
and expressions as `.qd`. They are checked with the model and rendered by the server,
with no browser and no external program. The same data and the same code always give the
same bytes.

```text
documents/invoice.qdp      document InvoicePdf(inv: Invoice), attach … to Invoice.pdf
documents/label.qdp        label ParcelLabel(o: Order)
```

| What | Declared | Gives |
|---|---|---|
| A document | `document InvoicePdf(inv: Invoice):` | a PDF: `render(InvoicePdf(inv))` |
| A label | `label ParcelLabel(o: Order) width 100 height 150 dpi 203:` | a PDF of the label's size, `render(…)`, or ZPL for thermal printers, `zpl(…)` |
| Kept with a record | `attach InvoicePdf to Invoice.pdf when state is Posted` | the document rendered after the commit, its hash in the record |
| Signed | `sign(…)`, `p7m(…)`, `xades(…)`, `pgp_sign(…)`, `pgp_encrypt(…)` | PAdES, CAdES, XAdES, OpenPGP |

## A document

```quadra
# documents/invoice.qdp: the invoice as the customer receives it.

from invoices import Invoice


document InvoicePdf(inv: Invoice):
    let number = inv.number ?? "draft"
    title "Invoice " + number
    page A4 margin 18
    font "Libertinus Serif" size 10
    header:
        row widths [2, 1]:
            text "Beans & Leaves" size 16 bold
            text "Via Roma 1, 20121 Milano" align right size 8
    footer:
        pages "Page {page} of {pages}" align center size 8
    heading "Invoice " + number level 1
    text inv.customer.name bold
    if let a = inv.customer.address:
        text a.street
    space 6
    table inv.lines as l:
        column "Product" width 3: l.product.name
        column "Qty" align right: l.qty
        column "Price" align right: l.price
        column "Amount" align right: l.amount
    totals:
        item "Subtotal": inv.subtotal
        item "VAT": inv.vat
        item "Total" bold: inv.total
    barcode "https://beans.example.com/i/" + number type qr height 22 align right
    if inv.payment_status is Paid:
        text "Paid, thank you." italic
    else:
        text "Due: " + f"{inv.due}"
```

A document is a function of its parameters. Its body is a list of **elements**, one per
line: the element, its value, its options, and for some a block or a `:` and what it shows.
Values are shown as everywhere in Quadra: `46.30 EUR`, `5 kg`, `18.00 EUR/kg`, a date
`2026-11-02`, a record by its number or name.

| Element | Value | Options | |
|---|---|---|---|
| `title` | text | | the PDF's title (once, at the top) |
| `page` | `A3`, `A4`, `A5`, `A6`, `Letter`, `Legal` | `landscape`, `margin` | once, at the top; A4 with 20 mm margins when not said |
| `font` | a font's name | `size` | the font and size of the text (Libertinus Serif, 10 pt when not said) |
| `header:`, `footer:` | | | repeated on every page |
| `heading` | text | `level` (1 to 3), `align`, `color` | |
| `text` | any value | `size`, `bold`, `italic`, `align`, `color`, `font` | a paragraph; `\n` starts a line |
| `pages` | `"Page {page} of {pages}"` | `size`, `align`, `color` | the page numbers, known once laid out |
| `row:` | | `widths [2, 1]`, `gap`, `valign` | its elements side by side, in columns |
| `block:` | | `width`, `fill`, `align`, `size`, `color` | its elements in a box (with `fill`, a coloured background) |
| `table … as x:` | a list | `size`, `header false` | a row per item; its `column "Title" width 2 align right: value` |
| `totals:` | | `width`, `size` | lines `item "Label" bold: value`, aligned right |
| `image` | a file of the project, `"assets/logo.png"` | `width`, `height`, `align` | `.png`, `.jpg` or `.svg` |
| `barcode` | text | `type`, `width`, `height`, `human`, `align` | `code128`, `ean13`, `gs1_128`, `sscc`, `qr`, `datamatrix` |
| `space` | millimetres | | vertical space |
| `line` | | `thickness`, `color` | a horizontal rule |
| `pagebreak` | | | |
| `typst` | Typst markup, as written | | an escape hatch: see below |

And among them, as in code: `let name = value`, `if condition:` / `else:`, `if let a =
optional:`, `for x in list:`.

Lengths are millimetres (`margin 18`, `width 40`), sizes points (`size 10`), colours
`"#1f4e79"`. A table goes on to the next page with its header repeated.

**Checked with the model.** Every expression is typed with the parameters: rename
`Invoice.subtotal` and the document is an error at its line, in `quadra check` and in the
editor, not a blank on a printed page. An element that does not exist, an option it does not
take, a table without `as`, a barcode without `type`: errors, with what is expected.

**Data is never markup.** Each value goes into the page as text: a customer called
`#[x] "special"` is printed as written, and cannot change the layout.

**`typst`**: the documents are laid out by [Typst](https://typst.app), embedded in the server
as a library, with its fonts in the binary (Libertinus Serif, New Computer Modern, DejaVu
Sans Mono). `typst "#lorem(30)"` puts Typst markup as written, a constant text, for what the
elements do not cover.

### `render(…)`

```quadra
let pdf = render(InvoicePdf(inv))
```

| Field | |
|---|---|
| `pdf.name` | `Invoice-INV-2026-00001.pdf`, from the title |
| `pdf.type` | `application/pdf` |
| `pdf.pages` | the pages |
| `pdf.size` | the bytes |
| `pdf.hash` | the SHA-256 of the bytes, in hex |
| `pdf.text` | the text of the pages (for tests) |

A document is rendered **after the commit**, never in a transaction: in an `outbound … as
e:` block, with `attach`, and in tests. In an action, a formula or a reaction it is an error:
`` `render` renders a document, which takes time: it runs after the commit, in an `outbound … as e:` block, or with `attach InvoicePdf to …` ``.

The same document with the same data gives the same bytes: no date, no random identifier.
What depends on the clock is what the document shows (`today()`).

## Sending and keeping documents

In an outbound block, a rendered document can be attached to an email, written as a file,
or printed ([printers](/reference/connections#printers-kind-printer): `print(zpl(ParcelLabel(e.order)))`):

```quadra
outbound InvoicePosted via mail as e:
    let pdf = render(InvoicePdf(e.invoice))
    email(to=e.invoice.customer.email ?? "", subject="Your invoice " + e.number,
        body="Please find your invoice attached.", attachments=[pdf])

outbound InvoicePosted via archive as e:
    write(f"invoices/{e.number}.pdf", render(InvoicePdf(e.invoice)))
```

### `attach`: kept with the record

```quadra
# in documents/invoice.qdp
attach InvoicePdf to Invoice.pdf signed "company" when state is Posted
```

```quadra
# in invoices.qd
entity Invoice:
    # …
    pdf: File?                              # the invoice as sent, once posted
```

| Rule | Detail |
|---|---|
| When | after each commit, each record of the entity for which `when` holds and whose field is still empty is rendered, from the data as it is then |
| Kept | the bytes once in the store of files ([Files](/reference/types#files), by their SHA-256), what they are in `_quadra_files`, the field set to `sha256:<hex>` in a transaction of its own, as the system (`attach:InvoicePdf Invoice#45`) |
| Frozen | the field is filled after the record is frozen: an `attach` field is an exception of `frozen when` by itself |
| `signed "company"` | the PDF is signed (PAdES) by that signer before it is kept |
| Once | the field, once filled, is never rendered again: the record keeps the document as it was sent, with its hash, even when the document's code changes later |
| A server that stops | between the render and the write, nothing is lost: the next pass finds the same records |
| A document that fails | (a bug, an image missing) said in the server's log and not tried again until the code changes |
| Where | `attach` goes in the `.qdp` file of its document (or a `.qd` module that imports the document and the entity: not the entity's own module, which the document imports) |

The document is downloaded by whoever may read the field:

```bash
curl -H "Authorization: Bearer $TOKEN" -o invoice.pdf \
  https://shop.quadra.run/v1/records/Invoice/45/files/pdf
```

with its type, its name (`Content-Disposition`) and its hash (`ETag`). Rendered again a year
later with the same code, the document gives the same hash as the one in the record.

## Labels

```quadra
# documents/label.qdp: the label on each parcel, for the thermal printer.

from orders import Order


label ParcelLabel(o: Order) width 100 height 150 dpi 203:
    let number = o.number ?? ""
    text "Beans & Leaves" x 5 y 5 size 10 bold
    text "Via Roma 1, 20121 Milano" x 5 y 10 size 8
    line x 5 y 16 x2 95 y2 16 thickness 0.4
    text o.customer.name x 5 y 22 size 16 bold width 90
    if let a = o.ship_to:
        text a.street x 5 y 32 size 12 width 90
        text a.zip + " " + a.city x 5 y 39 size 12 width 90
    text "Order " + number x 5 y 55 size 12
    barcode "37610425002123456" type sscc x 10 y 70 height 30 human
    barcode "https://beans.example.com/track/" + number type qr x 65 y 110 height 30
    box x 2 y 2 width 96 height 146 thickness 0.5
```

A label has a size in millimetres and the resolution of its printer (`dpi`: 152, 203, 300
or 600; 203 when not said). Its elements are placed, `x` and `y` in millimetres from the top
left:

| Element | Options |
|---|---|
| `text` | `x`, `y`, `size`, `bold`, `width` (the text wraps in it; to the right edge when not said), `align` |
| `barcode` | `x`, `y`, `type`, `height`, `width` (the width of a module), `human` (the digits under it) |
| `box` | `x`, `y`, `width`, `height`, `thickness` |
| `line` | `x`, `y`, `x2`, `y2`, `thickness` |
| `image` | `x`, `y`, `width`, `height` (a `.png` of the project) |

The same label gives two outputs:

| | |
|---|---|
| `render(ParcelLabel(o))` | a PDF of the label's size, for a laser printer or to look at |
| `zpl(ParcelLabel(o))` | ZPL, for Zebra and compatible thermal printers: positions in dots at the label's `dpi`, barcodes with the printer's own commands (`^BC`, `^BE`, `^BQ`, `^BX`) |

Every element must be inside the label, the quiet zone of a barcode included: one that is
not is an error that names it (`label 1, element 9 (SSCC …): the barcode with its quiet
zone (5 mm) starts 3 mm left of the label`). Text in a label is in the Western European
character set (Windows-1252).

### Barcodes

| `type` | Value | |
|---|---|---|
| `code128` | any text in ASCII | |
| `ean13` | 12 digits (the check digit is added) or 13 (checked) | `4006381333931` |
| `gs1_128` | GS1 element strings, `"(01)09501101530003(17)261231(10)LOT42"` | the AIs, their lengths and check digits are checked; FNC1 where GS1 wants it |
| `sscc` | the 17 digits of an SSCC (the check digit is added) or 18 | printed as GS1-128 with AI (00) |
| `qr` | any text | error correction M |
| `datamatrix` | any text; GS1 when it starts with `(` | ECC 200 |

A barcode that cannot be made says why: `EAN-13 takes 12 or 13 digits, not 11`, `the check
digit of 4006381333932 should be 1`.

## Signatures

The keys are in `quadra.toml`, from secret variables: never in the code, never readable by
it.

```toml
[vars]
SIGN_P12 = { secret = true, description = "the company's signing certificate (.p12, base64)" }
SIGN_PASSWORD = { secret = true }
BANK_PGP_KEY = { secret = true }

[signers.company]
p12 = { var = "SIGN_P12" }              # the .p12 file, in base64 (base64 -w0 < company.p12)
password = { var = "SIGN_PASSWORD" }
reason = "Invoice"                      # in the PDF's signature
location = "Milano"

[signers.treasury]
pgp_key = { var = "BANK_PGP_KEY" }      # an OpenPGP secret key, armored (passphrase = { var = … } if it has one)

[signers.bank]
pgp_public = """-----BEGIN PGP PUBLIC KEY BLOCK-----
…"""                                    # the bank's public key: files are encrypted for it
```

| Function | Signs | Gives |
|---|---|---|
| `sign(pdf, "company")` | a PDF rendered | the PDF signed, PAdES B-B (an incremental update: the content is unchanged) |
| `p7m(x, "company")` | a document, a record `from xml`, or text | `name.p7m`: CAdES-BES, the signed content inside (the FatturaPA `.xml.p7m`) |
| `xades(x, "company")` | a record `from xml`, or its text | the XML with its XAdES-BES enveloped signature |
| `pgp_sign(x, "treasury")` | a document or text | the detached OpenPGP signature, armored |
| `pgp_encrypt(x, "bank")` | a document or text | `name.pgp`, encrypted for the bank's key |

```quadra
record FatturaPa from xml(root="p:FatturaElettronica"):
    ns p "http://ivaservizi.agenziaentrate.gov.it/docs/xsd/fatture/v1.2"
    number: Text at "FatturaElettronicaBody/DatiGenerali/DatiGeneraliDocumento/Numero"
    total: Decimal at "FatturaElettronicaBody/DatiGenerali/DatiGeneraliDocumento/ImportoTotaleDocumento" decimals 2

outbound InvoicePosted via sdi as e:
    let x = FatturaPa(number=e.number, total=e.invoice.total / 1 EUR)
    write(f"out/{e.number}.xml.p7m", p7m(x, "company"))
```

| Rule | Detail |
|---|---|
| Where | like `render`: in outbound blocks and in tests |
| The time | the signing time is the clock's: in tests the test's, so the same document signed in a test gives the same bytes (OpenPGP encryption excepted: its session key is random) |
| Certificates | PKCS#12 as OpenSSL writes them (AES-256 and PBKDF2, or the legacy 3DES and RC2); RSA or EC P-256 keys; a certificate expired at the signing time is refused, so is a key that does not match it |
| Checked | the signatures are accepted by `pdfsig` (Poppler), `openssl cms -verify` and `gpg`; a byte changed and every one refuses |

## In tests

```quadra
from documents.invoice import InvoicePdf
from documents.label import ParcelLabel


test "an invoice renders":
    use demo
    let inv = cafe.orders.flat_map(o => o.invoices).first() ?? fail "no invoice"
    let pdf = render(InvoicePdf(inv))
    expect pdf.pages == 1
    expect pdf.text.contains("Caffè Centrale")
    expect render(InvoicePdf(inv)).hash == pdf.hash


test "a posted invoice keeps its PDF":
    use demo
    let inv = cafe.orders.flat_map(o => o.invoices).first() ?? fail "no invoice"
    expect inv.pdf == "sha256:" + sign(render(InvoicePdf(inv)), "company").hash


test "a parcel label, as PDF and as ZPL":
    use shop
    let o = Order(customer=anna, lines=[OrderLine(product=mug, qty=2 pcs)])
    o.confirm()
    expect render(ParcelLabel(o)).text.contains("Anna Rossi")
    let z = zpl(ParcelLabel(o))
    expect z.starts_with("^XA")
    expect z.contains(">;>800376104250021234569")      # the SSCC, with its check digit
```

In tests the `attach`es run after each statement, as the server runs them after each commit.
A document whose look must not change is compared by its hash: `expect pdf.hash ==
"8ce239bc…"` fails when anything in it changed. The signers' secrets take their `test`
values in tests (a certificate made for tests, never the real one).
