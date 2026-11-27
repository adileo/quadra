//! The formats read and written: round trips and the edge cases of each one.

use super::*;
use quadra_sema::{Project, Severity};
use std::path::Path;

fn model(src: &str) -> Model {
    let root = Path::new("/formats");
    let p = Project::from_sources(root, vec![(root.join("main.qd"), src.to_string())]);
    let checked = quadra_analysis::check(&p);
    let errors: Vec<String> = checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message.clone())
        .collect();
    assert!(errors.is_empty(), "{errors:#?}");
    checked.model.unwrap()
}

fn rows(v: &V) -> Vec<V> {
    match v {
        V::List(items) => items.to_vec(),
        other => panic!("not a list: {other:?}"),
    }
}

fn get(v: &V, name: &str) -> V {
    match v {
        V::Rec { fields, .. } => fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .unwrap_or(V::None),
        other => panic!("not a record: {other:?}"),
    }
}

fn err<T: std::fmt::Debug>(r: Result<T, String>) -> String {
    r.expect_err("an error was expected")
}

// ---- CSV ---------------------------------------------------------------------------------

const POS: &str = r#"
enum Pay:
    Cash, Card

record Row from csv(sep=";", decimal=","):
    code: Code at "CODE"
    name: Text at "NAME"
    qty: Int at "QTY"
    price: Decimal at "PRICE" decimals 2
    pay: Pay at "PAY"
    paid: Bool at "PAID"
    note: Text? at "NOTE"

record Latin from csv(encoding="latin1"):
    name: Text

record Tabs from csv(sep="\t", header=false):
    a: Text
    b: Int
"#;

#[test]
fn csv_quotes_separators_and_new_lines_round_trip() {
    let m = model(POS);
    let text = "CODE;NAME;QTY;PRICE;PAY;PAID;NOTE\r\n\
                A1;\"semi;colon\";2;3,5;Cash;true;\r\n\
                A2;\"say \"\"hi\"\"\";-1;0,10;Card;0;\"two\r\nlines\"\r\n";
    let v = parse_rows(&m, "Row", text).unwrap();
    let r = rows(&v);
    assert_eq!(r.len(), 2);
    assert_eq!(get(&r[0], "name"), V::Text("semi;colon".into()));
    assert_eq!(get(&r[1], "name"), V::Text("say \"hi\"".into()));
    assert_eq!(get(&r[1], "note"), V::Text("two\r\nlines".into()));
    assert_eq!(get(&r[0], "note"), V::None);
    assert_eq!(get(&r[1], "qty"), V::Int(-1));
    assert_eq!(get(&r[1], "paid"), V::Bool(false));
    let out = render_rows(&m, "Row", &r).unwrap();
    assert_eq!(
        out,
        "CODE;NAME;QTY;PRICE;PAY;PAID;NOTE\r\n\
         A1;\"semi;colon\";2;3,50;Cash;true;\r\n\
         A2;\"say \"\"hi\"\"\";-1;0,10;Card;false;\"two\r\nlines\"\r\n"
    );
    assert_eq!(parse_rows(&m, "Row", &out).unwrap(), v);
}

#[test]
fn csv_columns_are_found_by_the_header_in_any_order() {
    let m = model(POS);
    let text = "EXTRA,PAID,PAY,PRICE,QTY,NAME,CODE\nx,1,Card,\"1,5\",3,n,C\n";
    // the separator of this record is `;`: the whole line is one column
    assert!(err(parse_rows(&m, "Row", text)).contains("the header has no column `CODE`"));
    let text = "EXTRA;PAID;PAY;PRICE;QTY;NAME;CODE\nx;1;Card;1,5;3;n;C\n";
    let r = rows(&parse_rows(&m, "Row", text).unwrap());
    assert_eq!(get(&r[0], "code"), V::Text("C".into()));
    assert_eq!(get(&r[0], "price"), V::Dec(Decimal::new(15, 1)));
    // NOTE is optional: the header may not have it
    assert_eq!(get(&r[0], "note"), V::None);
}

#[test]
fn csv_errors_say_the_line_of_the_row_and_the_column() {
    let m = model(POS);
    // the second row starts on line 4: the cell before it spans two lines
    let text = "CODE;NAME;QTY;PRICE;PAY;PAID;NOTE\nA;\"x\ny\";1;1;Cash;1;\nB;z;one;1;Cash;1;\n";
    assert_eq!(
        err(parse_rows(&m, "Row", text)),
        "line 4, column `QTY`: an integer was expected, not `one`"
    );
    let text = "CODE;NAME;QTY;PRICE;PAY;PAID;NOTE\nA;\"x\"y;1;1;Cash;1;\n";
    assert!(err(parse_rows(&m, "Row", text)).starts_with("line 2: text after the closing quote"));
    let text = "CODE;NAME;QTY;PRICE;PAY;PAID;NOTE\nA;x;1;;Cash;1;\n";
    assert_eq!(
        err(parse_rows(&m, "Row", text)),
        "line 2, column `PRICE`: missing"
    );
    let text = "CODE;NAME;QTY;PRICE;PAY;PAID;NOTE\nA;x;1;1;Cash;maybe;\n";
    assert_eq!(
        err(parse_rows(&m, "Row", text)),
        "line 2, column `PAID`: true or false was expected, not `maybe`"
    );
}

#[test]
fn csv_formulas_are_neutralized_in_text_not_in_numbers() {
    let m = model(POS);
    let text = "CODE;NAME;QTY;PRICE;PAY;PAID;NOTE\n-A;'=1+1;-3;-2,00;Cash;1;+39 02\n";
    let r = rows(&parse_rows(&m, "Row", text).unwrap());
    // the `'` of a neutralized formula is not part of the text
    assert_eq!(get(&r[0], "name"), V::Text("=1+1".into()));
    let out = render_rows(&m, "Row", &r).unwrap();
    assert_eq!(
        out.lines().nth(1).unwrap(),
        "'-A;'=1+1;-3;-2,00;Cash;true;'+39 02"
    );
    assert_eq!(parse_rows(&m, "Row", &out).unwrap(), V::List(Rc::new(r)));
}

#[test]
fn csv_in_latin1_is_decoded_and_encoded() {
    let m = model(POS);
    let bytes = b"name\r\nCaff\xe8 \xab\xbb\r\n";
    let v = parse_rows_bytes(&m, "Latin", bytes).unwrap();
    assert_eq!(get(&rows(&v)[0], "name"), V::Text("Caffè «»".into()));
    assert_eq!(
        render_rows_bytes(&m, "Latin", &rows(&v)).unwrap(),
        bytes.to_vec()
    );
    // the same bytes are not UTF-8
    assert_eq!(
        err(parse_rows_bytes(&m, "Row", b"CODE\r\nCaff\xe8\r\n")),
        "line 2: not UTF-8 (a file in latin1 says `encoding=\"latin1\"`)"
    );
    // a character latin1 cannot hold is refused where it is
    let euro = parse_rows(&m, "Latin", "name\n5 €\n").unwrap();
    assert_eq!(
        err(render_rows(&m, "Latin", &rows(&euro))),
        "row 1, column `name`: `€` cannot be written in latin1 (ISO-8859-1)"
    );
    assert!(err(encode("ok\n€", Encoding::Latin1)).starts_with("line 2: `€`"));
}

#[test]
fn csv_without_a_header_and_other_separators() {
    let m = model(POS);
    let v = parse_rows(&m, "Tabs", "a b\t1\n\n\"x\ty\"\t-2").unwrap();
    let r = rows(&v);
    assert_eq!(r.len(), 2);
    assert_eq!(get(&r[1], "a"), V::Text("x\ty".into()));
    assert_eq!(
        render_rows(&m, "Tabs", &r).unwrap(),
        "a b\t1\r\n\"x\ty\"\t-2\r\n"
    );
    // a byte order mark is not part of the first cell
    let v = parse_rows(&m, "Tabs", "\u{feff}a\t1\n").unwrap();
    assert_eq!(get(&rows(&v)[0], "a"), V::Text("a".into()));
}

// ---- XML ---------------------------------------------------------------------------------

const XML: &str = r#"
record Party from xml:
    ns cbc "urn:b"
    name: Text at "cbc:Name"
    vat: Text? at "cbc:CompanyID"

record Amount from xml:
    value: Decimal at "."
    currency: Code at "@currencyID"

record Doc from xml(root="Invoice"):
    ns "urn:inv"
    ns cbc "urn:b"
    ns cac "urn:a"
    id: Text at "cbc:ID"
    issued: Date at "cbc:IssueDate" format "%d/%m/%Y"
    paid: Bool at "cbc:Paid" format "Y/N"
    supplier: Party at "cac:Supplier/cac:Party"
    total: Amount at "cbc:Total"
    notes: [Text] at "cbc:Note"
    tags: [Text] at "Tags/Tag/@name"
    due: Date? at "cbc:DueDate"

record Loose from xml:
    name: Text
    count: Int? at "Count"
"#;

const DOC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- a comment -->
<Invoice xmlns="urn:inv" xmlns:b="urn:b" xmlns:a="urn:a">
  <b:ID>F &lt;1&gt; &#x41;</b:ID>
  <b:IssueDate>27/11/2026</b:IssueDate>
  <b:Paid>N</b:Paid>
  <a:Supplier><a:Party><b:Name>Rossi &amp; Figli</b:Name></a:Party></a:Supplier>
  <b:Total currencyID="EUR"> 12.50 </b:Total>
  <b:Note>first</b:Note>
  <b:Note><![CDATA[<second>]]></b:Note>
  <Tags><Tag name="a"/><Tag name="b"/></Tags>
  <Ignored>anything</Ignored>
</Invoice>"#;

#[test]
fn xml_is_read_by_namespace_not_by_prefix() {
    let m = model(XML);
    let v = parse(&m, "Doc", DOC).unwrap();
    assert_eq!(get(&v, "id"), V::Text("F <1> A".into()));
    assert_eq!(
        get(&v, "issued"),
        V::Date(NaiveDate::from_ymd_opt(2026, 11, 27).unwrap())
    );
    assert_eq!(get(&v, "paid"), V::Bool(false));
    assert_eq!(
        get(&get(&v, "supplier"), "name"),
        V::Text("Rossi & Figli".into())
    );
    assert_eq!(get(&get(&v, "supplier"), "vat"), V::None);
    assert_eq!(
        get(&get(&v, "total"), "value"),
        V::Dec(Decimal::new(1250, 2))
    );
    assert_eq!(
        get(&v, "notes"),
        V::List(Rc::new(vec![
            V::Text("first".into()),
            V::Text("<second>".into())
        ]))
    );
    assert_eq!(
        get(&v, "tags"),
        V::List(Rc::new(vec![V::Text("a".into()), V::Text("b".into())]))
    );
    assert_eq!(get(&v, "due"), V::None);
}

#[test]
fn xml_round_trip_declares_the_namespaces() {
    let m = model(XML);
    let v = parse(&m, "Doc", DOC).unwrap();
    let (out, ctype) = render(&m, &v).unwrap();
    assert_eq!(ctype, "application/xml");
    assert_eq!(
        out,
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <Invoice xmlns=\"urn:inv\" xmlns:cbc=\"urn:b\" xmlns:cac=\"urn:a\">\
         <cbc:ID>F &lt;1&gt; A</cbc:ID><cbc:IssueDate>27/11/2026</cbc:IssueDate><cbc:Paid>N</cbc:Paid>\
         <cac:Supplier><cac:Party><cbc:Name>Rossi &amp; Figli</cbc:Name></cac:Party></cac:Supplier>\
         <cbc:Total currencyID=\"EUR\">12.5</cbc:Total>\
         <cbc:Note>first</cbc:Note><cbc:Note>&lt;second&gt;</cbc:Note>\
         <Tags><Tag name=\"a\"/><Tag name=\"b\"/></Tags></Invoice>"
    );
    assert_eq!(parse(&m, "Doc", &out).unwrap(), v);
}

#[test]
fn xml_written_without_a_default_namespace_inside_one() {
    // `Party` has no default namespace: an unprefixed element of it is in none, even inside
    // a document whose default namespace is another
    let m = model(
        r#"
record Inner from xml:
    code: Text at "Code"

record Outer from xml:
    ns "urn:outer"
    inner: Inner at "Inner"
"#,
    );
    let text = "<Outer xmlns=\"urn:outer\"><Inner><Code xmlns=\"\">7</Code></Inner></Outer>";
    let v = parse(&m, "Outer", text).unwrap();
    assert_eq!(get(&get(&v, "inner"), "code"), V::Text("7".into()));
    let (out, _) = render(&m, &v).unwrap();
    assert!(out.ends_with(text), "{out}");
    // the element in the wrong namespace is not the field
    let wrong = "<Outer xmlns=\"urn:outer\"><Inner><Code>7</Code></Inner></Outer>";
    assert_eq!(err(parse(&m, "Outer", wrong)), "Outer/Inner/Code: missing");
}

#[test]
fn xml_refuses_a_dtd_and_unknown_entities() {
    let m = model(XML);
    let xxe = "<?xml version=\"1.0\"?>\n<!DOCTYPE Loose [<!ENTITY xxe SYSTEM \"file:///etc/passwd\">]>\n<Loose><name>&xxe;</name></Loose>";
    assert!(err(parse(&m, "Loose", xxe)).contains("line 2: a DTD (`<!DOCTYPE …>`) is refused"));
    let laughs = "<?xml version=\"1.0\"?>\n<!DOCTYPE lolz [\n <!ENTITY lol \"lol\">\n <!ENTITY lol2 \"&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;\">\n]>\n<Loose><name>&lol2;</name></Loose>";
    assert!(err(parse(&m, "Loose", laughs)).contains("a DTD"));
    let undeclared = "<Loose><name>&lol;</name></Loose>";
    assert!(err(parse(&m, "Loose", undeclared)).contains("`&lol;` is not an entity of XML"));
}

#[test]
fn xml_refuses_documents_too_deep_or_too_large() {
    let m = model(XML);
    let deep = format!(
        "<Loose>{}{}</Loose>",
        "<a>".repeat(XML_MAX_DEPTH + 1),
        "</a>".repeat(XML_MAX_DEPTH + 1)
    );
    assert!(err(parse(&m, "Loose", &deep)).contains("nested deeper than 64 levels"));
    let large = format!("<Loose><name>{}</name></Loose>", "x".repeat(XML_MAX_BYTES));
    assert!(err(parse(&m, "Loose", &large)).contains("more than the"));
    let many = format!("<Loose>{}</Loose>", "<a/>".repeat(xml::MAX_ELEMENTS + 1));
    assert!(err(parse(&m, "Loose", &many)).contains("more than 200000 elements"));
}

#[test]
fn xml_errors_say_where() {
    let m = model(XML);
    assert_eq!(
        err(parse(
            &m,
            "Loose",
            "<Loose><name>x</name><Count>many</Count></Loose>"
        )),
        "Loose/Count: an integer was expected, not `many`"
    );
    assert_eq!(
        err(parse(&m, "Loose", "<Loose><Count>1</Count></Loose>")),
        "Loose/name: missing"
    );
    // an empty element is none for an optional number
    let v = parse(&m, "Loose", "<Loose><name/><Count/></Loose>").unwrap();
    assert_eq!(get(&v, "name"), V::Text("".into()));
    assert_eq!(get(&v, "count"), V::None);
    assert!(err(parse(&m, "Loose", "<Loose><name>x</Loose>")).contains("not XML"));
    assert!(err(parse(&m, "Loose", "<Loose/><Loose/>")).contains("a second root element"));
    assert!(err(parse(&m, "Loose", "")).contains("no element"));
    assert!(
        err(parse(&m, "Doc", "<Order xmlns=\"urn:inv\"/>")).contains("the root element is `Order`")
    );
    // a character XML cannot hold is refused when written
    let v = parse(&m, "Loose", "<Loose><name>x</name></Loose>").unwrap();
    let V::Rec { ent, fields } = v else { panic!() };
    let mut fields = fields.to_vec();
    fields[0].1 = V::Text("bell \u{7}".into());
    let bad = V::Rec {
        ent,
        fields: Rc::new(fields),
    };
    assert_eq!(
        err(render(&m, &bad)),
        "Loose/name: the character U+0007 cannot be written in XML"
    );
}

// ---- fixed-width ---------------------------------------------------------------------------

const FIXED: &str = r#"
record Line from fixed(encoding="latin1"):
    kind: Code at "1..2"
    name: Text at "3..10"
    qty: Int at "11..15"
    amount: Decimal at "16..23" decimals 2
    flag: Bool at "24"
    day: Date? at "25..30" format "%d%m%y"
    note: Text? at "33..36"
"#;

#[test]
fn fixed_width_pads_and_reads_back() {
    let m = model(FIXED);
    let text = "01Rossi   00042-00012501241126  note\r\n02Bianchi -0007000000000      \n";
    let v = parse_rows(&m, "Line", text).unwrap();
    let r = rows(&v);
    assert_eq!(r.len(), 2);
    assert_eq!(get(&r[0], "name"), V::Text("Rossi".into()));
    assert_eq!(get(&r[0], "qty"), V::Int(42));
    assert_eq!(get(&r[0], "amount"), V::Dec(Decimal::new(-1250, 2)));
    assert_eq!(get(&r[0], "flag"), V::Bool(true));
    assert_eq!(get(&r[0], "note"), V::Text("note".into()));
    assert_eq!(get(&r[1], "qty"), V::Int(-7));
    assert_eq!(get(&r[1], "day"), V::None);
    assert_eq!(get(&r[1], "note"), V::None);
    let out = render_rows(&m, "Line", &r).unwrap();
    assert_eq!(
        out,
        "01Rossi   00042-00012501241126  note\r\n02Bianchi -0007000000000            \r\n"
    );
    assert_eq!(parse_rows(&m, "Line", &out).unwrap(), v);
}

#[test]
fn fixed_width_refuses_what_does_not_fit() {
    let m = model(FIXED);
    let v = parse_rows(&m, "Line", "01Rossi   00001000001001").unwrap();
    let with = |name: &str, value: V| -> Vec<V> {
        let V::Rec { ent, fields } = &rows(&v)[0] else {
            panic!()
        };
        let mut fields = fields.to_vec();
        for f in fields.iter_mut() {
            if f.0 == name {
                f.1 = value.clone();
            }
        }
        vec![V::Rec {
            ent: ent.clone(),
            fields: Rc::new(fields),
        }]
    };
    assert_eq!(
        err(render_rows(
            &m,
            "Line",
            &with("name", V::Text("Bartolomeo".into()))
        )),
        "row 1, columns 3..10 (`name`): `Bartolomeo` is 10 characters, the columns hold 8"
    );
    assert_eq!(
        err(render_rows(&m, "Line", &with("qty", V::Int(123456)))),
        "row 1, columns 11..15 (`qty`): 123456 does not fit in 5 columns"
    );
    assert_eq!(
        err(render_rows(&m, "Line", &with("qty", V::Int(-12345)))),
        "row 1, columns 11..15 (`qty`): -12345 does not fit in 5 columns"
    );
    assert_eq!(
        err(render_rows(
            &m,
            "Line",
            &with("amount", V::Dec(Decimal::new(1005, 3)))
        )),
        "row 1, columns 16..23 (`amount`): 1.005 has more than 2 decimals (round it first)"
    );
    assert_eq!(
        err(render_rows(
            &m,
            "Line",
            &with("name", V::Text("a\nb".into()))
        )),
        "row 1, columns 3..10 (`name`): a new line cannot be in a fixed-width line"
    );
    assert_eq!(
        err(render_rows(
            &m,
            "Line",
            &with("name", V::Text("5 €".into()))
        )),
        "row 1, columns 3..10 (`name`): `€` cannot be written in latin1 (ISO-8859-1)"
    );
    // a short line goes on with spaces: the number is missing
    assert_eq!(
        err(parse_rows(&m, "Line", "01Rossi")),
        "line 1, columns 11..15 (`qty`): missing (only spaces)"
    );
    assert_eq!(
        err(parse_rows(&m, "Line", "\n01Rossi   0000100000,00")),
        "line 2, columns 16..23 (`amount`): digits were expected, not `00000,00` (the last 2 are the decimals, without a separator)"
    );
}

// ---- JSON ----------------------------------------------------------------------------------

#[test]
fn json_dates_booleans_and_decimals_follow_their_modifiers() {
    let m = model(
        r#"
record Order from json:
    day: Date format "%d.%m.%Y"
    gift: Bool format "yes/no"
    total: Decimal decimals 2
    lines: [Decimal] decimals 3
"#,
    );
    let v = parse(
        &m,
        "Order",
        r#"{"day":"27.11.2026","gift":"no","total":"12.5","lines":[1,2.25]}"#,
    )
    .unwrap();
    assert_eq!(get(&v, "gift"), V::Bool(false));
    let (out, ctype) = render(&m, &v).unwrap();
    assert_eq!(ctype, "application/json");
    assert_eq!(
        out,
        r#"{"day":"27.11.2026","gift":"no","total":12.50,"lines":[1.000,2.250]}"#
    );
    assert_eq!(parse(&m, "Order", &out).unwrap(), v);
    assert_eq!(
        err(parse(
            &m,
            "Order",
            r#"{"day":"2026-11-27","gift":"no","total":1,"lines":[]}"#
        )),
        "Order.day: a date (27.11.2026) was expected, not `2026-11-27`"
    );
}
