//! The editor's hints (`quadra_store::hints`): how queries, row rules and formulas meet the
//! database, from the model alone.

use quadra_sema::Severity;

fn hints_of(src: &str) -> Vec<(usize, String)> {
    let dir =
        std::env::temp_dir().join(format!("quadra-hints-{}-{}", std::process::id(), src.len()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("quadra.toml"), "[project]\nname = \"hints\"\n").unwrap();
    std::fs::write(dir.join("main.qd"), src).unwrap();
    let project = quadra_sema::Project::load(&dir).unwrap();
    let checked = quadra_analysis::check(&project);
    let errors: Vec<String> = checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message.clone())
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    let out = quadra_store::hints::hints(&checked)
        .into_iter()
        .filter(|d| project.file(d.file).kind != quadra_sema::FileKind::Std)
        .map(|d| {
            assert_eq!(d.severity, Severity::Hint);
            let text = &project.file(d.file).text;
            (
                text[..usize::from(d.range.start())].matches('\n').count() + 1,
                d.message,
            )
        })
        .collect();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn on(h: &[(usize, String)], line: usize) -> Vec<String> {
    h.iter()
        .filter(|(l, _)| *l == line)
        .map(|(_, m)| m.clone())
        .collect()
}

const SRC: &str = r#"
entity Region:
    code: Code unique
    name: Text
    locked: Bool default false

entity Customer:
    code: Code unique
    name: Text
    region: Region
    owner: User?
    score: Int default 0
    orders: [Order]
    open = orders.count(o => o.state is Open)
    lucky = Order.count(o => o.total > score)

entity Order:
    number: Code unique
    customer: Customer
    state: enum(Open, Closed) default Open
    total: Int default 0
    note: Text?

    action review:
        let a = Customer.where(c => c.region == self.customer.region).count()
        let b = Order.count(o => o.total > 100)
        let c = Customer.where(c => c.name.len() > self.total).first()
        let d = Order.where(o => o.customer.region.locked).map(o => o.number)
        let e = Order.where(o => fancy(o.total)).count()
        for x in Order.where(o => o.state is Closed):
            x.note = "seen"

fn fancy(n: Int) -> Bool:
    return n > 3

role Clerk:
    read Region, Order
    read Customer where c => c.owner == me or c.open > 2
"#;

#[test]
fn hints_say_how_queries_rules_and_formulas_run() {
    let h = hints_of(SRC);
    let all: Vec<String> = h.iter().map(|(l, m)| format!("{l}: {m}")).collect();
    let joined = all.join("\n");
    // a reference compared with a value: an index
    assert!(
        on(&h, 25).iter().any(|m| m.contains(
            "`Customer.where(…).count()` runs in SQL using an index (reference `Customer.region`)"
        )),
        "{joined}"
    );
    // a field without an index: every row read
    assert!(
        on(&h, 26)
            .iter()
            .any(|m| m.contains("`Order.count(…)` runs in SQL but reads every row of `order`")),
        "{joined}"
    );
    assert!(
        on(&h, 27)
            .iter()
            .any(|m| m.contains("`Customer.where(…).first()` runs in SQL")),
        "{joined}"
    );
    // a path through two references; then `.map` in the interpreter
    assert!(on(&h, 28).iter().any(|m| m.contains("runs in SQL") && m.contains("`.map` then runs in the interpreter")), "{joined}");
    // a user function: the interpreter
    assert!(
        on(&h, 29).iter().any(
            |m| m.contains("runs in the interpreter over every Order record")
                && m.contains("does not translate")
        ),
        "{joined}"
    );
    // the loop's query
    assert!(
        on(&h, 30)
            .iter()
            .any(|m| m.contains("`Order.where(…)` runs in SQL")),
        "{joined}"
    );
    // one hint per query: the chain `Customer.where(…).count()` is described once
    assert_eq!(on(&h, 25).len(), 1, "{joined}");
    // a rule reading a formula: the interpreter, row by row
    assert!(
        on(&h, 38)
            .iter()
            .any(|m| m.contains("applied by the interpreter, row by row")
                && m.contains("`Customer.open` is a formula")),
        "{joined}"
    );
    // a formula reading a whole entity: computed when read
    assert!(
        on(&h, 15)
            .iter()
            .any(|m| m.contains("`Customer.lucky` is computed when read")
                && m.contains("it reads every Order")),
        "{joined}"
    );
    // an incremental formula has no hint
    assert!(on(&h, 14).is_empty(), "{joined}");
}
