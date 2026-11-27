//! Time zones: a project in `Europe/Rome` on a machine in any zone (the clock of the tests is
//! fake). The criteria of the cloud's phase 8: `today()` turns at the Italian midnight, summer
//! time included; a daily 9:00 stays at 9:00 across the change of the clocks; `+ 1 day` and
//! `+ 24 hours` differ that night; a record in another zone computes its local date in it.

use quadra_sema::Project;

fn project(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("quadra-tz-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (path, text) in files {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    dir
}

fn run(dir: &std::path::Path) {
    let p = Project::load(dir).unwrap();
    let results = match quadra_runtime::run_tests(&p, None) {
        Ok(r) => r,
        Err(d) => panic!(
            "the project has errors: {:?}",
            d.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
        ),
    };
    let failed: Vec<String> = results
        .iter()
        .filter(|t| !t.passed)
        .map(|t| {
            format!(
                "{} ({}): {}",
                t.name,
                t.location,
                t.message.clone().unwrap_or_default()
            )
        })
        .collect();
    assert!(!results.is_empty());
    assert!(failed.is_empty(), "failed:\n{}", failed.join("\n"));
}

const MODEL: &str = r#"
entity Customer:
    name: Text
    timezone: Text

entity Reminder:
    customer: Customer
    at: DateTime
    # the day it falls on where the customer is, not where the project is
    local_day = at.in(customer.timezone).date
    local_hour = at.in(customer.timezone).hour
"#;

const TESTS: &str = r#"
test "today() turns at midnight in Rome, in winter and in summer":
    clock 2026-01-15 23:59
    expect today() == 2026-01-15
    expect now().in("UTC").hour == 22
    clock 2026-01-16 00:00
    expect today() == 2026-01-16
    expect today("UTC") == 2026-01-15
    clock 2026-07-15 23:59
    expect today() == 2026-07-15
    expect now().in("UTC").hour == 21
    clock 2026-07-16 00:00
    expect today() == 2026-07-16
    expect today("America/New_York") == 2026-07-15

test "an instant's hour and weekday are read on the project's clock":
    clock 2026-11-24 09:30
    expect now().hour == 9
    expect now().minute == 30
    expect now().weekday == 2
    expect now().date == 2026-11-24
    expect now().in("Asia/Tokyo").hour == 17
    expect now().in("Europe/Rome").offset == 1 hour
    expect 2026-07-01T12:00.in("Europe/Rome").offset == 2 hours

test "9:00 every day stays at 9:00 across the change of the clocks":
    # the night of 25 October 2026 is 25 hours long in Rome
    let nine = datetime(2026-10-24, "09:00")
    expect nine.hour == 9
    expect (nine + 1 day).hour == 9
    expect (nine + 1 day).date == 2026-10-25
    expect (nine + 24 hours).hour == 8
    expect (nine + 1 day) - nine == 25 hours
    expect datetime(2026-10-26, "09:00") - datetime(2026-10-25, "09:00") == 24 hours
    # and in spring the night is 23 hours long
    expect datetime(2026-03-29, "09:00") - datetime(2026-03-28, "09:00") == 23 hours
    expect (datetime(2026-03-28, "09:00") + 1 week).hour == 9

test "a time that does not exist moves forward; one that happens twice is the first":
    expect datetime(2026-03-29, "02:30").hour == 3
    expect datetime(2026-03-29, "02:30").in("UTC").hour == 1
    expect datetime(2026-10-25, "02:30").in("UTC").hour == 0

test "a record in another zone computes its local date there":
    let c = Customer(name="Acme NY", timezone="America/New_York")
    # 2026-11-24 03:00 in Rome is still the 23rd in New York (21:00)
    let r = Reminder(customer=c, at=2026-11-24T03:00)
    expect r.local_day == 2026-11-23
    expect r.local_hour == 21
    expect r.at.date == 2026-11-24
    expect datetime(2026-11-24, "09:00", c.timezone).in("Europe/Rome").hour == 15

test "a literal date-time is a time of the project's clock":
    expect 2026-11-24T10:00.in("UTC").hour == 9
    expect 2026-11-24T10:00 == datetime(2026-11-24, "10:00")
    expect 2026-11-24T10:00 == datetime(2026-11-24, "09:00", "UTC")
"#;

#[test]
fn the_project_zone_and_explicit_zones() {
    let dir = project(
        "rome",
        &[
            (
                "quadra.toml",
                "[project]\nname = \"tz\"\ntimezone = \"Europe/Rome\"\n",
            ),
            ("main.qd", MODEL),
            ("tests/time.test.qd", TESTS),
        ],
    );
    run(&dir);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn without_a_zone_the_project_is_in_utc() {
    let dir = project("utc", &[
        ("quadra.toml", "[project]\nname = \"utc\"\n"),
        ("main.qd", "entity A:\n    n: Int\n"),
        (
            "tests/utc.test.qd",
            "test \"utc\":\n    clock 2026-07-15 23:59\n    expect today() == 2026-07-15\n    expect now().in(\"UTC\").hour == 23\n    expect today(\"Europe/Rome\") == 2026-07-16\n",
        ),
    ]);
    run(&dir);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_zone_that_does_not_exist_is_refused() {
    let dir = project(
        "bad",
        &[
            (
                "quadra.toml",
                "[project]\nname = \"bad\"\ntimezone = \"Europe/Atlantis\"\n",
            ),
            ("main.qd", "entity A:\n    n: Int\n"),
        ],
    );
    let e = Project::load(&dir).err().expect("refused");
    assert!(e.to_string().contains("Europe/Atlantis"), "{e}");
    std::fs::write(dir.join("quadra.toml"), "[project]\nname = \"bad\"\n").unwrap();
    std::fs::write(
        dir.join("main.qd"),
        "entity A:\n    at: DateTime\n    d = at.in(\"Mars/Base\").date\n",
    )
    .unwrap();
    let p = Project::load(&dir).unwrap();
    let c = quadra_analysis::check(&p);
    assert!(
        c.diagnostics
            .iter()
            .any(|d| d.message.contains("Mars/Base")),
        "{:?}",
        c.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
