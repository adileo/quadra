//! `document` and `label` (`.qdp` files) and `attach`.

const INVOICE: &str = r##"from invoices import Invoice

document InvoicePdf(inv: Invoice):
    title inv.number ?? "draft"
    page A4 margin 18
    font "Libertinus Serif" size 10
    header:
        row widths [2, 1]:
            image "assets/logo.png" width 40
            text "Beans & Leaves" align right bold
    footer:
        pages "Page {page} of {pages}" align center size 8
    let n = inv.number ?? ""
    heading f"Invoice {n}" level 1
    text inv.customer.name bold
    table inv.lines as l:
        column "Product" width 3: l.product.name
        column "Amount" align right: l.amount
    totals:
        item "Total" bold: inv.total
    if inv.paid > 0 EUR:
        text "Paid in part"
    else:
        text "Not paid yet" italic
    for p in inv.payments:
        text p.amount
    barcode n type code128 height 12
    line
    pagebreak
    typst "#lorem(10)"

label ParcelLabel(inv: Invoice) width 100 height 150 dpi 203:
    text inv.customer.name x 5 y 5 size 14 bold width 90
    barcode "00106141411234567" type sscc x 5 y 40 height 25 human
    box x 2 y 2 width 96 height 146 thickness 0.5

attach InvoicePdf to Invoice.pdf when state is Posted
"##;

#[test]
fn a_document_a_label_and_an_attach_parse() {
    let p = quadra_syntax::parse(INVOICE);
    assert!(p.errors.is_empty(), "{:?}\n{}", p.errors, p.debug_tree());
    let tree = p.debug_tree();
    assert_eq!(tree.matches("DOCUMENT@").count(), 2, "{tree}");
    assert_eq!(tree.matches("ATTACH@").count(), 1);
    assert_eq!(tree.matches("PRINT_IF@").count(), 1);
    assert_eq!(tree.matches("PRINT_FOR@").count(), 1);
    assert!(tree.matches("PRINT_OPT@").count() >= 20);
}

#[test]
fn a_line_that_is_not_an_element_is_reported() {
    let p = quadra_syntax::parse("document D(x: Int):\n    42\n    text x\n");
    assert_eq!(p.errors.len(), 1, "{:?}", p.errors);
    assert!(p.errors[0].message.contains("expected an element"));
}
