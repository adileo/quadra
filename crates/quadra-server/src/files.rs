//! The files of rows an `inbound` reads (a record `from csv` or `from fixed`, pulled from an
//! sftp or ftp connection): `atomic per file` (the default), all the rows in one transaction
//! or none; `atomic per row`, each row that works, the others refused. Either way what was
//! wrong is a report, written next to the file where it is moved (`<file>.report.txt`).
//!
//! | | A row that cannot be read | A row whose code fails |
//! |---|---|---|
//! | per file | the file is `rejected`, no row written; the report lists every such line | the file is `rejected`, no row written; the report says that row |
//! | per row | refused, in the report | refused, in the report; the rows before it in its chunk are committed without it |
//!
//! Per row, the message ends `done` with the report of the refused rows, or `rejected` when
//! every row was refused. A server stopped halfway through a file goes on after the chunks
//! already committed (`quadra_store::deliveries::rows_handled`).

use quadra_ir::{Atomic, Inbound, Model};
use quadra_store::Conn;
use quadra_types::Report;

/// The most rows a report lists, the others counted.
const LISTED: usize = 1000;

/// Whether a record is a row of a file (`from csv`, `from fixed`).
pub(crate) fn rows_record(model: &Model, ty: &str) -> bool {
    model
        .entity(ty)
        .is_some_and(|e| matches!(e.format.as_deref(), Some("csv" | "fixed")))
}

/// What a row is called in a report: a line, or a record of a file in EBCDIC.
fn unit(model: &Model, ty: &str) -> &'static str {
    let ebcdic = model
        .entity(ty)
        .and_then(|e| quadra_ir::formats::options(e).ok())
        .is_some_and(|o| matches!(o.encoding, quadra_ir::formats::Encoding::Ebcdic(_)));
    if ebcdic {
        "record"
    } else {
        "line"
    }
}

/// The report: a first line that says what happened, then a line per row refused.
fn report(head: String, refused: &[(usize, String)]) -> String {
    let mut out = head;
    for (_, why) in refused.iter().take(LISTED) {
        out.push('\n');
        out.push_str(why);
    }
    if refused.len() > LISTED {
        out.push_str(&format!("\n… and {} more", refused.len() - LISTED));
    }
    out.push('\n');
    out
}

fn rows(n: usize) -> String {
    if n == 1 {
        "1 row".into()
    } else {
        format!("{n} rows")
    }
}

/// Handles the bytes of message `id`, a file of rows of `inbound`: the transaction (or the
/// error, when it is rejected), and the report of the rows refused.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_file(
    conn: &mut Conn,
    url: &str,
    sequencer: Option<&quadra_store::Sequencer>,
    model: &Model,
    checked: &Report,
    at: chrono::NaiveDateTime,
    i: &Inbound,
    id: u64,
    bytes: &[u8],
) -> (Result<u64, String>, Option<String>) {
    let unit = unit(model, &i.ty);
    let each = match quadra_runtime::formats::read_rows(model, &i.ty, bytes) {
        Ok(e) => e,
        Err(e) => {
            let e = format!("{}: {e}", i.ty);
            return (Err(e.clone()), Some(format!("{e}\n")));
        }
    };
    let total = each.len();
    let mut good = Vec::with_capacity(total);
    let mut refused: Vec<(usize, String)> = Vec::new();
    for (n, r) in each {
        match r {
            Ok(v) => good.push((n, v)),
            Err(e) => refused.push((n, e)),
        }
    }
    let per_row = i.atomic == Some(Atomic::Row);
    if !per_row && !refused.is_empty() {
        let head = format!(
            "{}: the file is not applied, no row was written: {} of {} cannot be read",
            i.ty,
            rows(refused.len()),
            total
        );
        let first = format!("{head} ({})", refused[0].1);
        return (Err(first), Some(report(head, &refused)));
    }
    let (ranges, last) = if per_row {
        quadra_store::deliveries::rows_handled(conn, &i.name, id).unwrap_or_default()
    } else {
        Default::default()
    };
    let done = |n: usize| ranges.iter().any(|(a, b)| *a <= n && n <= *b);
    let before = good.iter().filter(|(n, _)| done(*n)).count();
    let outcome = quadra_store::ops::handle_inbound_rows(
        url,
        model,
        checked,
        at,
        sequencer,
        i,
        &good,
        per_row,
        &format!("inbound:{}#{id}", i.name),
        done,
        unit,
    );
    match outcome {
        Err(e) if per_row => (Err(e), None),
        Err(e) => {
            let head = format!("{}: the file is not applied, no row was written", i.ty);
            (Err(e.clone()), Some(format!("{head}\n{e}\n")))
        }
        Ok(o) => {
            let tx = o.tx.or(last).unwrap_or(0);
            refused.extend(
                o.refused
                    .into_iter()
                    .map(|(n, e)| (n, format!("{unit} {n}: {e}"))),
            );
            refused.sort_by_key(|(n, _)| *n);
            if refused.is_empty() {
                (Ok(tx), None)
            } else if o.committed + before == 0 {
                let head = format!("{}: every row was refused ({})", i.ty, rows(total));
                (
                    Err(format!("{head}: {}", refused[0].1)),
                    Some(report(head, &refused)),
                )
            } else {
                let head = format!(
                    "{}: {} of {} refused, the others are in",
                    i.ty,
                    rows(refused.len()),
                    total
                );
                (Ok(tx), Some(report(head, &refused)))
            }
        }
    }
}
