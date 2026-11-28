//! How long the messages received and the events sent are kept: once closed and past their
//! term, a message keeps only its thin row (key, hash, state) for one more term, then goes; an
//! event goes; what is pending, failed or rejected stays, and so does the latest of a lane.

use quadra_sema::Project;
use quadra_store::deliveries::{message_terms, prune_messages, to_archive};
use quadra_store::{ops, Conn};

fn at(s: &str) -> chrono::NaiveDateTime {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").unwrap()
}

#[test]
fn closed_messages_and_events_go_after_their_term() {
    let dir = std::env::temp_dir().join(format!("quadra-messages-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("p")).unwrap();
    std::fs::write(
        dir.join("p/quadra.toml"),
        "[project]\nname = \"m\"\n\n[data]\noutbox = \"10 days\"\n\n[connections.shop]\nkind = \"http\"\nurl = \"https://shop.example.com\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("p/main.qd"),
        "record ShopOrder from json:\n    id: Text\n\nentity Order:\n    code: Text\n\nevent OrderShipped(code: Text)\n\ninbound OrderIn via shop path \"/orders\" as m: ShopOrder:\n    keep 30 days, then archive\n    Order(code=m.id)\n\noutbound OrderShipped via shop path \"/hooks/orders\" as e:\n    retry 3 times\n",
    )
    .unwrap();
    let p = Project::load(&dir.join("p")).unwrap();
    let checked = quadra_analysis::check(&p);
    assert!(
        checked.model.is_some(),
        "{:?}",
        checked
            .diagnostics
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );
    let (m, r) = (checked.model.as_ref().unwrap(), &checked.report);
    let url = format!("sqlite:{}", dir.join("data.db").display());
    ops::init(&url, m, r, at("2026-01-01T00:00:00")).unwrap();
    let mut c = Conn::open(&url).unwrap();
    // (id, state, received/done at)
    for (id, state, when) in [
        (1, "done", "2026-01-02T00:00:00"),     // closed long ago: goes
        (2, "done", "2026-02-20T00:00:00"),     // closed 40 days ago: emptied, stays thin
        (3, "done", "2026-03-30T00:00:00"),     // recent: stays
        (4, "rejected", "2026-01-02T00:00:00"), // waits for a person
        (5, "received", "2026-01-02T00:00:00"), // pending
    ] {
        c.exec(&format!("INSERT INTO \"_quadra_inbox\" (\"id\", \"inbound\", \"connection\", \"key\", \"hash\", \"body\", \"received_at\", \"state\", \"done_at\") VALUES ({id}, 'OrderIn', 'shop', 'k{id}', 'h{id}', '{{\"id\":\"{id}\"}}', '{when}', '{state}', '{when}')"), &[]).unwrap();
    }
    for (id, state, lane, when) in [
        (1, "delivered", None, "2026-03-01T00:00:00"),
        (2, "delivered", Some("a"), "2026-03-01T00:00:00"),
        (3, "delivered", Some("a"), "2026-03-02T00:00:00"), // the latest of lane a: stays
        (4, "failed", None, "2026-03-01T00:00:00"),
        (5, "delivered", None, "2026-03-31T00:00:00"),
    ] {
        let lane = lane.map(|l| format!("'{l}'")).unwrap_or("NULL".into());
        c.exec(&format!("INSERT INTO \"_quadra_outbox\" (\"id\", \"tx\", \"event\", \"fields\", \"created_at\", \"delivered\", \"delivered_at\", \"state\", \"lane\") VALUES ({id}, 1, 'OrderShipped', '{{}}', '{when}', true, '{when}', '{state}', {lane})"), &[]).unwrap();
    }
    let (inbox, outbox) = message_terms(m, at("2026-04-01T00:00:00"));
    let archived = to_archive(&mut c, &inbox, &outbox, 100).unwrap();
    assert_eq!(archived.len(), 2, "{archived:?}");
    let (emptied, deleted) = prune_messages(&mut c, &inbox, &outbox).unwrap();
    let ids = |c: &mut Conn, sql: &str| -> Vec<String> {
        c.query(sql, &[])
            .unwrap()
            .into_iter()
            .map(|r| r[0].clone().unwrap_or_default())
            .collect()
    };
    assert_eq!(
        ids(
            &mut c,
            "SELECT CAST(\"id\" AS TEXT) || ':' || \"body\" FROM \"_quadra_inbox\" ORDER BY \"id\""
        ),
        vec![
            "2:",
            "3:{\"id\":\"3\"}",
            "4:{\"id\":\"4\"}",
            "5:{\"id\":\"5\"}"
        ]
    );
    assert_eq!(
        ids(
            &mut c,
            "SELECT CAST(\"id\" AS TEXT) FROM \"_quadra_outbox\" ORDER BY \"id\""
        ),
        vec!["3", "4", "5"]
    );
    assert_eq!((emptied, deleted), (1, 3));
    let _ = std::fs::remove_dir_all(&dir);
}
