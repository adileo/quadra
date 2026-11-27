//! The statements of a test about integrations: `receive`, `deliver`, `expect sent`.

use quadra_sema::Project;
use std::path::PathBuf;

const TOML: &str = "[project]\nname = \"i\"\n\n[vars]\nSHOP_TOKEN = { secret = true }\nSHOP_HOOKS = { secret = true }\n\n[connections.shop]\nkind = \"http\"\nurl = \"https://shop.example.com\"\nbearer = { var = \"SHOP_TOKEN\" }\ninbound_signature = \"shopify\"\ninbound_secret = { var = \"SHOP_HOOKS\" }\n";

const MAIN: &str = r#"entity Product:
    sku: Text unique
    price: Decimal
    shop_id: Text?

entity Order:
    number: Text
    total: Decimal
    tracking: Text?

record ShopOrder from json:
    id: Text
    number: Text at "name"
    total: Decimal at "total_price"

record ShopProduct from json:
    sku: Text
    price: Decimal

record ShopCreated from json:
    id: Text

record Ack from json:
    tracking: Text

event OrderPlaced(order: Order)

on Order created as o:
    emit OrderPlaced(order=o)

outbound OrderPlaced via shop as e:
    reply Ack
    post("/fulfil", e.order.number)

on OrderPlaced delivered as d:
    d.event.order.tracking = d.reply?.tracking

on OrderPlaced failed as f:
    d_failed(f.event.order, f.error)

fn d_failed(o: Order, why: Text):
    o.tracking = f"failed: {why}"

fn place_and_fail(n: Text):
    let o = Order(number=n, total=1)
    fail "boom"

inbound OrderIn via shop path "/orders" as m: ShopOrder:
    key m.id
    if m.total < 0:
        fail "a negative total"
    let o = Order(number=m.number, total=m.total)

publish Product to shop as p:
    reply ShopCreated
    if let id = p.shop_id:
        put(f"/products/{id}", ShopProduct(sku=p.sku, price=p.price))
    else:
        post("/products", ShopProduct(sku=p.sku, price=p.price))

on PublishProductToShop delivered as d:
    d.event.record.shop_id = d.event.record.shop_id ?? d.reply?.id
"#;

const TESTS: &str = r##"test "a message comes in once, and what it makes goes out":
    receive OrderIn "{\"id\": \"1001\", \"name\": \"#1001\", \"total_price\": \"129.90\"}"
    receive OrderIn file "tests/order-1001.json"
    expect Order.count() == 1
    let o = Order.where(x => x.number == "#1001").first() ?? fail "no order"
    expect sent OrderPlaced(order=o)
    expect sent OrderPlaced
    deliver OrderPlaced reply Ack(tracking="TRK-1")
    expect o.tracking == "TRK-1"
    expect_error "no OrderPlaced waiting":
        deliver OrderPlaced

test "a bad message is rejected and changes nothing":
    expect_error "a negative total":
        receive OrderIn "{\"id\": \"7\", \"name\": \"#7\", \"total_price\": \"-1\"}"
    expect_error "is not a ShopOrder":
        receive OrderIn "{\"id\": \"8\"}"
    expect Order.count() == 0
    expect not sent OrderPlaced

test "an event of a transaction undone never goes":
    expect_error "boom":
        place_and_fail("Z1")
    expect Order.count() == 0
    expect not sent OrderPlaced

test "a failure runs its reaction":
    let o = Order(number="A1", total=1)
    deliver OrderPlaced failed "503: busy"
    expect o.tracking == "failed: 503: busy"

test "a published record keeps the shop's id and is not sent back":
    let p = Product(sku="P1", price=10)
    expect sent PublishProductToShop(record=p)
    deliver PublishProductToShop reply ShopCreated(id="sh-1")
    expect p.shop_id == "sh-1"
    p.price = 12
    deliver PublishProductToShop reply ShopCreated(id="ignored")
    expect p.shop_id == "sh-1"
    expect_error "no PublishProductToShop waiting":
        deliver PublishProductToShop
"##;

fn project(tests: &str) -> (PathBuf, Project) {
    let dir = std::env::temp_dir().join(format!(
        "quadra-integrations-{}-{}",
        std::process::id(),
        tests.len()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(dir.join("quadra.toml"), TOML).unwrap();
    std::fs::write(dir.join("main.qd"), MAIN).unwrap();
    std::fs::write(dir.join("tests/integrations.test.qd"), tests).unwrap();
    std::fs::write(
        dir.join("tests/order-1001.json"),
        r##"{"id": "1001", "name": "#1001", "total_price": "129.90"}"##,
    )
    .unwrap();
    let p = Project::load(&dir).unwrap();
    (dir, p)
}

fn results(tests: &str) -> Vec<(String, Option<String>)> {
    let (dir, p) = project(tests);
    let r = quadra_runtime::run_tests(&p, None).unwrap_or_else(|d| {
        panic!(
            "does not check:\n{}",
            d.iter()
                .map(|d| d.message.clone())
                .collect::<Vec<_>>()
                .join("\n")
        )
    });
    let _ = std::fs::remove_dir_all(dir);
    r.into_iter().map(|r| (r.name, r.message)).collect()
}

#[test]
fn receive_deliver_and_expect_sent_play_the_other_side() {
    let r = results(TESTS);
    assert_eq!(r.len(), 5);
    for (name, message) in r {
        assert!(message.is_none(), "{name}: {message:?}");
    }
}

#[test]
fn expect_sent_says_what_was_sent_instead() {
    let r = results("test \"wrong\":\n    let o = Order(number=\"A1\", total=1)\n    let other = Order(number=\"A2\", total=2)\n    expect sent OrderPlaced(order=o)\n    expect not sent OrderPlaced(order=o)\n");
    let m = r[0].1.clone().unwrap();
    assert!(m.contains("expected no OrderPlaced(order=o)"), "{m}");
    assert!(m.contains("sent: OrderPlaced(order="), "{m}");
}

const FILES_TOML: &str = "[project]\nname = \"f\"\n\n[vars]\nPOS_PASSWORD = { secret = true }\n\n[connections.pos]\nkind = \"sftp\"\nurl = \"sftp://files.example.com\"\nuser = \"quadra\"\npassword = { var = \"POS_PASSWORD\" }\nknown_host = \"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl\"\n";

fn files_project(main: &str, files: &[(&str, &[u8])]) -> (PathBuf, Result<Project, String>) {
    let dir = std::env::temp_dir().join(format!(
        "quadra-integrations-files-{}-{}",
        std::process::id(),
        main.len()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(dir.join("quadra.toml"), FILES_TOML).unwrap();
    std::fs::write(dir.join("main.qd"), main).unwrap();
    for (name, body) in files {
        std::fs::write(dir.join("tests").join(name), body).unwrap();
    }
    let p = Project::load(&dir).map_err(|e| e.to_string());
    (dir, p)
}

const SALES: &str = "entity Sale:\n    sku: Code\n    qty: Int\n\nrecord PosRow from csv(sep=\";\"):\n    sku: Code at \"SKU\"\n    qty: Int at \"QTA\"\n\n";

fn closing(atomic: &str) -> String {
    format!("{SALES}inbound Closing via pos as r: PosRow:\n    pull \"in/*.csv\" every 5 minutes\n{atomic}    if r.qty == 0:\n        fail \"a quantity of zero\"\n    let s = Sale(sku=r.sku, qty=r.qty)\n")
}

#[test]
fn a_file_of_rows_is_received_whole_or_row_by_row() {
    let files: &[(&str, &[u8])] = &[
        ("good.csv", b"SKU;QTA\nA;1\nB;2\n"),
        ("zero.csv", b"SKU;QTA\nA;1\nB;0\nC;3\n"),
        ("unreadable.csv", b"SKU;QTA\nA;x\n"),
        ("all-zero.csv", b"SKU;QTA\nA;0\n"),
        ("mixed.csv", b"SKU;QTA\nD;x\nE;4\n"),
    ];
    let per_file = format!("{}\ntest \"per file\":\n    receive Closing file \"tests/good.csv\"\n    expect Sale.count() == 2\n    expect_error \"line 3: a quantity of zero\":\n        receive Closing file \"tests/zero.csv\"\n    expect_error \"line 2, column `QTA`\":\n        receive Closing file \"tests/unreadable.csv\"\n    expect Sale.count() == 2\n", closing("    atomic per file\n"));
    let per_row = format!("{}\ntest \"per row\":\n    receive Closing file \"tests/zero.csv\"\n    expect Sale.count() == 2\n    receive Closing file \"tests/mixed.csv\"\n    expect_error \"every row was refused\":\n        receive Closing file \"tests/all-zero.csv\"\n    expect Sale.count() == 3\n", closing("    atomic per row\n"));
    for main in [per_file, per_row] {
        let (dir, p) = files_project(&main, files);
        let r = quadra_runtime::run_tests(&p.unwrap(), None).unwrap_or_else(|d| {
            panic!(
                "{:?}",
                d.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
            )
        });
        let _ = std::fs::remove_dir_all(dir);
        assert_eq!(r.len(), 1);
        assert!(r[0].message.is_none(), "{}: {:?}", r[0].name, r[0].message);
    }
}

#[test]
fn atomic_and_packed_numbers_are_refused_where_they_mean_nothing() {
    let refused = |main: &str| -> Vec<String> {
        let (dir, p) = files_project(main, &[]);
        let checked = quadra_analysis::check(&p.unwrap());
        let _ = std::fs::remove_dir_all(dir);
        checked
            .diagnostics
            .iter()
            .filter(|d| d.severity == quadra_sema::Severity::Error)
            .map(|d| d.message.clone())
            .collect()
    };
    let json = "entity Sale:\n    sku: Code\n\nrecord Doc from json:\n    sku: Code\n\ninbound DocIn via pos as d: Doc:\n    pull \"in/*.json\" every 5 minutes\n    atomic per row\n    let s = Sale(sku=d.sku)\n";
    let e = refused(json);
    assert!(
        e.iter()
            .any(|m| m.contains("`atomic per …` says how the rows of a file are committed")),
        "{e:?}"
    );
    let e = refused("record Row from fixed:\n    amount: Decimal at \"1..4\" packed decimals 2\n");
    assert!(
        e.iter()
            .any(|m| m.contains("`packed` numbers are in fixed-width files in EBCDIC")),
        "{e:?}"
    );
    let e =
        refused("record Row from fixed(encoding=\"ibm-280\"):\n    name: Text at \"1..4\" zoned\n");
    assert!(
        e.iter()
            .any(|m| m.contains("`zoned` is a way of writing an `Int` or a `Decimal`")),
        "{e:?}"
    );
    let e = refused(
        "record Row from fixed(encoding=\"ibm-280\", length=3):\n    qty: Int at \"1..4\" zoned\n",
    );
    assert!(
        e.iter()
            .any(|m| m.contains("go past the end of the record (`length=3`)")),
        "{e:?}"
    );
    let e = refused("record Row from csv(encoding=\"ibm-280\"):\n    qty: Int\n");
    assert!(
        e.iter()
            .any(|m| m.contains("EBCDIC is read in fixed-width files")),
        "{e:?}"
    );
    let e = refused("record Row from fixed(encoding=\"ibm-999\"):\n    qty: Int at \"1..4\"\n");
    assert!(e.iter().any(|m| m.contains("\"ibm-1144\"")), "{e:?}");
    assert!(refused("record Row from fixed(encoding=\"IBM280\", length=10):\n    qty: Int at \"1..4\" zoned\n    amount: Decimal? at \"5..8\" packed decimals 2\n").is_empty());
}
