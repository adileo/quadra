//! `quadra bench`: the stress benchmark of `examples/stress`.
//!
//!     quadra bench gen --scale small [--seed 1] --out DIR
//!     quadra bench run [path] --url URL --scale small [--json out.json] [--timeout 60] …
//!
//! `run` loads generated data with the bulk import, then measures each scenario in a child
//! process of its own (`quadra bench step <name> …`): the peak memory is the scenario's, and
//! a scenario that does not finish in time (at most 60 s) is killed and reported as "> N s"
//! instead of hanging the run. Each scenario has a target (the notes on scaling, Goal): it is
//! `ok` or `fail`; `--gate a,b` (or `all`) makes the run exit non-zero when one of those fails. Scenarios go through the same code as the server (`quadra_server::Local`:
//! `POST /v1/query`, `GET /v1/records`, `POST /v1/tx`) and as `quadra dev` (`ops::deploy`).

use crate::stress;
use quadra_store::conn::{Conn, Dialect, Sql};
use serde_json::{json, Value as J};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// (name, what it measures). In the order they run: reads before writes, schema changes
/// last (each builds on the previous one: the database ends on a changed schema).
pub const STEPS: &[(&str, &str)] = &[
    ("load", "bulk load: generate, validate, formulas, write (db init + import)"),
    ("grid", "Shipment grid: first page of 500 with total, as Admin"),
    ("grid_where1", "Shipment grid where line.qty >= 15 pcs (1 relation)"),
    ("grid_where3", "Shipment grid where line.order.customer.code == … (3 relations)"),
    ("grid_sorted", "Shipment grid ordered by date desc"),
    ("grid_sales", "Shipment grid as a Salesperson (read rule 4 relations up)"),
    ("grid_logistics", "Shipment grid as Logistics (shipment → line → order → customer → region.logistics == me)"),
    ("grid_manager", "Order grid as a RegionalManager (inherited role, rules joined by or)"),
    ("grid_deny", "Order grid as a Controller (read *, deny read through 2 relations)"),
    ("agg_sales", "Shipment aggregates (sum, max, count) and total as a Salesperson"),
    ("search_sales", "Customer search \"rossi\" as a Salesperson (3 rules: salesperson, manager, team)"),
    ("ref_picker_sales", "reference picker: Customers as a Salesperson, no fields"),
    ("grid_lines", "OrderLine grid: first page (rollups of shipments per row)"),
    ("record_order", "open a confirmed Order"),
    ("record_customer", "open a Customer (rollups over its orders)"),
    ("record_region", "open a Region (rollups of rollups over its subtree, its customers bounded)"),
    ("record_region_more", "the next page of a Region's customers (the record's cursor, `refs`)"),
    ("record_frozen", "open a cancelled Order (frozen: formulas as of then)"),
    ("commit_qty", "commit OrderLine.qty (rollups 4 levels up, checks)"),
    ("commit_shipment", "commit a new Shipment (checks 4 relations away)"),
    ("kanban_move", "a card dragged on the Order board: Draft -> Confirmed through its transition (`confirm`), as Admin"),
    ("commit_move", "commit Customer.region (moves its orders, lines and shipments to another region)"),
    ("commit_category", "commit Product.category (the units sold of two categories, by delta)"),
    ("commit_tree", "commit Category.parent (the depth and path of its subtree, by recursion)"),
    ("move_read", "move a customer to another region, then the Order grid as its new manager (the read timed)"),
    ("agg_region", "revenue per region: Region grid with its rollups"),
    ("agg_top_customers", "top 10 customers by revenue"),
    ("search_prefix", "Customer search: name.starts_with(\"Rossi\"), 50 rows"),
    ("ref_picker", "reference picker: 1000 Customers, no fields"),
    ("scroll", "page 50 of the Shipment grid filtered by carrier, by date desc (keyset cursor)"),
    ("query_action", "an action with Customer.where, Order.count, .sum, .sort_by(…).first() and a loop over Order.where"),
    ("query_check", "a commit whose check counts Order.where (refused: nothing written)"),
    ("query_formula", "Region grid with a column counting Order.where per region"),
    ("group_region_month", "Order revenue (confirmed, shipped) grouped by customer.region × month"),
    ("group_sales", "Orders grouped by customer and state with their total, as a Salesperson"),
    ("pivot_category", "pivot: products' units sold and margin by category (rows) × active (columns)"),
    ("pivot_lines", "reference: pivot of the order lines' amount and quantity by product.category × order.state (every line read)"),
    ("fts_customer", "Customer grid with a full-text search \"rossi logis\" (every word a prefix), with total"),
    ("fts_sales", "Customer grid with a full-text search \"tyrell figli\", as a Salesperson"),
    ("kanban_orders", "Order board by state: the count and total of every column (one group query) and the first page of each column, as Admin"),
    ("kanban_sales", "the same Order board as a Salesperson (row rules in every column)"),
    ("sheet_eval", "an app's spreadsheet: 200 formulas over the database (20 regions × 10 figures) and a spill of 1000 orders, cold, as Admin"),
    ("sheet_recalc", "the same spreadsheet, one input cell changed: its 10 dependent formulas computed again, the others kept"),
    ("app_open", "open the Sales cockpit (examples/stress/apps): every component's first evaluation (KPIs, charts, the board, the pivot sheet), formulas cold, as Admin"),
    ("app_open_sales", "the same Sales cockpit opened by a Salesperson (row rules in every component)"),
    ("app_filter_change", "the cockpit's region filter changed: every component evaluated again (the checked formulas kept), as Admin"),
    ("app_action", "the cockpit's button: confirm the order selected on the board, then every component evaluated again, as Admin"),
    ("sql_count", "SQL: count of shipments"),
    ("sql_page_sorted", "SQL: 500 shipments by date desc"),
    ("sql_where3", "SQL: shipments of one customer (3 joins)"),
    ("sql_region_revenue", "SQL: revenue per region from the materialized column"),
    ("sql_top_customers", "SQL: top 10 customers by materialized revenue"),
    ("sql_prefix", "SQL: customers whose name starts with Rossi"),
    ("verify", "quadra db verify: the hash chain and every table against the history"),
    ("deploy_noop", "deploy of the same code (quadra dev, save without changes)"),
    ("deploy_add_field", "deploy: add Customer.segment: Text?"),
    ("deploy_add_default", "deploy: add Order.channel with a default (every order)"),
    ("deploy_rename", "deploy: rename Customer.name -> title, with its migration"),
    ("deploy_add_check", "deploy: add a check on OrderLine"),
    ("deploy_add_required", "deploy: Shipment.tracking required when qty > 1000 pcs (every shipment, in SQL)"),
    ("deploy_add_where", "deploy: a `where` on Shipment.tracking (every shipment, in SQL)"),
    ("deploy_add_unique", "deploy: unique (tracking) on Shipment (duplicates looked up in SQL)"),
];

/// What a scenario must achieve (the notes on scaling, Goal), measured cold.
#[derive(Clone, Copy, Debug)]
pub enum Target {
    /// at most this many milliseconds
    Ms(f64),
    /// at least this many rows per second (the bulk load)
    RowsPerSec(f64),
    /// a reference measurement: finishing in time is enough
    None,
}

pub fn target(name: &str) -> Target {
    match name {
        "load" => Target::RowsPerSec(50_000.0),
        "grid" | "grid_where1" | "grid_where3" | "grid_sorted" | "grid_lines" => Target::Ms(100.0),
        "grid_sales" | "grid_logistics" | "grid_manager" | "grid_deny" | "agg_sales"
        | "move_read" => Target::Ms(200.0),
        "search_sales" | "ref_picker_sales" => Target::Ms(100.0),
        // a page of a record's lines, as the panel scrolls
        "record_region_more" => Target::Ms(100.0),
        n if n.starts_with("record_") => Target::Ms(50.0),
        // its fan-out is the rollups of two regions (phase 3)
        "commit_move" => Target::Ms(200.0),
        n if n.starts_with("commit_") => Target::Ms(20.0),
        n if n.starts_with("agg_") => Target::Ms(300.0),
        // grouping and pivots: aggregations across tables
        "group_region_month" | "group_sales" | "pivot_category" => Target::Ms(300.0),
        "fts_customer" | "fts_sales" => Target::Ms(100.0),
        // a board: counts per column and a page of each column, cold
        "kanban_orders" | "kanban_sales" => Target::Ms(200.0),
        // an app's spreadsheet (POST /v1/eval): the whole workbook cold, one input changed
        "sheet_eval" => Target::Ms(500.0),
        "sheet_recalc" => Target::Ms(200.0),
        // an app: opened cold (< 1 s; medium: < 2 s, the notes on scaling), a filter changed, a button
        "app_open" | "app_open_sales" => Target::Ms(1000.0),
        "app_filter_change" | "app_action" => Target::Ms(300.0),
        "kanban_move" => Target::Ms(50.0),
        "search_prefix" | "ref_picker" | "scroll" => Target::Ms(100.0),
        // a commit whose check reads a whole entity
        "query_check" => Target::Ms(100.0),
        // queries across tables, as an aggregation
        "query_action" | "query_formula" => Target::Ms(300.0),
        // every record of the database checked against its history: within the time limit
        "verify" => Target::Ms(60_000.0),
        // a deploy that changes no data: seconds, whatever the number of rows
        "deploy_noop" | "deploy_add_field" | "deploy_rename" => Target::Ms(5_000.0),
        // a deploy that writes or checks every record of an entity: within the time limit
        "deploy_add_default"
        | "deploy_add_check"
        | "deploy_add_required"
        | "deploy_add_where"
        | "deploy_add_unique" => Target::Ms(60_000.0),
        _ => Target::None,
    }
}

fn target_text(t: Target) -> String {
    match t {
        Target::Ms(ms) if ms >= 1000.0 => format!("< {:.0} s", ms / 1000.0),
        Target::Ms(ms) => format!("< {ms:.0} ms"),
        Target::RowsPerSec(r) => format!("≥ {:.0} rows/s", r),
        Target::None => "—".into(),
    }
}

/// `ok` when the scenario finished in time and met its target, `fail` otherwise.
fn verdict(o: &Outcome) -> &'static str {
    if o.status != "ok" {
        return "fail";
    }
    let v = &o.last;
    match target(&o.name) {
        Target::Ms(ms) => match v["cold_ms"].as_f64() {
            Some(x) if x <= ms => "ok",
            _ => "fail",
        },
        Target::RowsPerSec(r) => match v["rows_per_s"].as_f64() {
            Some(x) if x >= r => "ok",
            _ => "fail",
        },
        Target::None => "ok",
    }
}

/// The longest a scenario may take: anything longer is a failure, never measured.
const MAX_TIMEOUT: u64 = 60;

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

const FLAGS: &[&str] = &[
    "--url",
    "--scale",
    "--seed",
    "--out",
    "--json",
    "--timeout",
    "--load-timeout",
    "--reps",
    "--only",
    "--skip",
    "--commit-size",
    "--gate",
];

pub fn path_arg(args: &[String]) -> PathBuf {
    args.iter()
        .enumerate()
        .find(|(i, a)| !a.starts_with("--") && (*i == 0 || !FLAGS.contains(&args[i - 1].as_str())))
        .map(|(_, a)| PathBuf::from(a))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn bench(args: &[String]) -> ExitCode {
    let r = match args.first().map(String::as_str) {
        Some("gen") => gen(&args[1..]),
        Some("run") => run(&args[1..]),
        Some("step") if args.len() > 1 => step(&args[1], &args[2..]),
        _ => Err(format!("usage: quadra bench gen --scale <{}> --out DIR [--seed N] | run [path] --url URL --scale <…> [--json file] [--timeout secs (≤ 60)] [--reps N] [--only a,b] [--skip a,b] [--gate a,b|all]", stress::scale_names())),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn seed(args: &[String]) -> u64 {
    flag(args, "--seed")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
}

fn gen(args: &[String]) -> Result<(), String> {
    let scale = flag(args, "--scale").ok_or("--scale is required")?;
    let out = flag(args, "--out").ok_or("--out DIR is required")?;
    let t = Instant::now();
    let mut sink = stress::CsvDir::new(Path::new(&out))?;
    let g = stress::generate(&scale, seed(args), &mut sink)?;
    sink.finish()?;
    for (e, n) in &g.counts {
        println!("  {e:<10} {n:>10}");
    }
    println!(
        "✓ {} rows in {out} ({:.1} s): load them with `quadra db import {out} examples/stress`",
        g.total(),
        t.elapsed().as_secs_f64()
    );
    Ok(())
}

// ---- measuring ---------------------------------------------------------------------------

/// Peak resident memory of this process, in MB (not measured on Windows: 0).
#[cfg(not(unix))]
pub fn peak_rss_mb() -> f64 {
    0.0
}

/// Peak resident memory of this process, in MB.
#[cfg(unix)]
pub fn peak_rss_mb() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage only writes the struct it is given
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let v = u.ru_maxrss as f64;
    if cfg!(target_os = "macos") {
        v / 1_048_576.0
    } else {
        v / 1024.0
    }
}

fn percentile(xs: &[f64], p: f64) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = ((p / 100.0) * (v.len() - 1) as f64).round() as usize;
    Some(v[i])
}

/// What a step reports, as a JSON line on stdout after every measurement: the parent keeps
/// the last one it saw, so a step killed by the timeout still reports what it finished.
struct Meter {
    cold: Option<f64>,
    warm: Vec<f64>,
    extra: serde_json::Map<String, J>,
}

impl Meter {
    fn new() -> Meter {
        Meter {
            cold: None,
            warm: Vec::new(),
            extra: serde_json::Map::new(),
        }
    }

    fn emit(&self, done: bool) {
        let mut o = self.extra.clone();
        o.insert("cold_ms".into(), json!(self.cold));
        o.insert("warm_ms".into(), json!(self.warm));
        o.insert("p50_ms".into(), json!(percentile(&self.warm, 50.0)));
        o.insert("p95_ms".into(), json!(percentile(&self.warm, 95.0)));
        o.insert(
            "rss_mb".into(),
            json!((peak_rss_mb() * 10.0).round() / 10.0),
        );
        o.insert("done".into(), json!(done));
        println!("{}", J::Object(o));
        let _ = std::io::stdout().flush();
    }

    fn set(&mut self, k: &str, v: J) {
        self.extra.insert(k.into(), v);
    }

    /// The first call is the cold one (empty caches), then `reps` warm calls. Each reports
    /// the statements it sent and the records it loaded from the database.
    fn time(
        &mut self,
        reps: usize,
        mut f: impl FnMut(&mut Meter, bool) -> Result<(), String>,
    ) -> Result<(), String> {
        let (s0, r0) = counters();
        let d0 = quadra_store::conn::db_micros();
        let t = Instant::now();
        f(self, true)?;
        self.cold = Some(ms(t));
        let (s1, r1) = counters();
        self.set("cold_statements", json!(s1 - s0));
        self.set("cold_records_loaded", json!(r1 - r0));
        // the cold call's time in the database (loading records, the cache's checks); the
        // rest is the engine (formulas, rules, JSON)
        self.set(
            "cold_db_ms",
            json!(((quadra_store::conn::db_micros() - d0) as f64 / 100.0).round() / 10.0),
        );
        self.emit(false);
        for _ in 0..reps {
            let (s0, r0) = counters();
            let t = Instant::now();
            f(self, false)?;
            self.warm.push(ms(t));
            let (s1, r1) = counters();
            self.set("warm_statements", json!(s1 - s0));
            self.set("warm_records_loaded", json!(r1 - r0));
            self.emit(false);
        }
        Ok(())
    }
}

/// (statements sent, records loaded) by this process so far.
fn counters() -> (u64, u64) {
    (
        quadra_store::conn::statements(),
        quadra_store::source::RECORDS_LOADED.load(std::sync::atomic::Ordering::Relaxed),
    )
}

fn ms(t: Instant) -> f64 {
    (t.elapsed().as_secs_f64() * 1e5).round() / 100.0
}

// ---- the run -----------------------------------------------------------------------------

struct Outcome {
    name: String,
    status: String,
    last: J,
    secs: f64,
}

fn run(args: &[String]) -> Result<(), String> {
    let path = path_arg(args);
    let url =
        flag(args, "--url").ok_or("--url is required (sqlite:path or postgres://…#schema=name)")?;
    let scale = flag(args, "--scale").ok_or("--scale is required")?;
    if !stress::SCALES.iter().any(|s| s.0 == scale) {
        return Err(format!(
            "unknown scale `{scale}` ({})",
            stress::scale_names()
        ));
    }
    let timeout: u64 = flag(args, "--timeout")
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_TIMEOUT);
    let load_timeout: u64 = flag(args, "--load-timeout")
        .and_then(|v| v.parse().ok())
        .unwrap_or(timeout);
    if timeout > MAX_TIMEOUT || load_timeout > MAX_TIMEOUT {
        return Err(format!("a scenario may take at most {MAX_TIMEOUT} s: anything longer is a failure (--timeout, --load-timeout)"));
    }
    let gate: Vec<String> = match flag(args, "--gate").as_deref() {
        Some("all") => STEPS.iter().map(|s| s.0.to_string()).collect(),
        Some(list) => list.split(',').map(str::to_string).collect(),
        None => Vec::new(),
    };
    if let Some(bad) = gate
        .iter()
        .find(|g| !STEPS.iter().any(|s| s.0 == g.as_str()))
    {
        return Err(format!("--gate: unknown scenario `{bad}`"));
    }
    let reps = flag(args, "--reps").unwrap_or_else(|| "5".into());
    let only: Option<Vec<String>> =
        flag(args, "--only").map(|s| s.split(',').map(str::to_string).collect());
    let skip: Vec<String> = flag(args, "--skip")
        .map(|s| s.split(',').map(str::to_string).collect())
        .unwrap_or_default();
    let steps: Vec<&(&str, &str)> = STEPS
        .iter()
        .filter(|(n, _)| {
            only.as_ref().map_or(true, |o| o.iter().any(|x| x == n)) && !skip.iter().any(|x| x == n)
        })
        .collect();
    if steps.iter().any(|(n, _)| *n == "load") {
        wipe(&url)?;
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut outcomes: Vec<Outcome> = Vec::new();
    eprintln!("quadra bench: scale {scale}, {url}, timeout {timeout} s per scenario\n");
    for (name, what) in steps {
        let limit = if *name == "load" {
            load_timeout
        } else {
            timeout
        };
        eprintln!("▶ {name}: {what}");
        let mut cmd = Command::new(&exe);
        cmd.arg("bench").arg("step").arg(name).arg(&path).args([
            "--url",
            &url,
            "--scale",
            &scale,
            "--seed",
            &seed(args).to_string(),
            "--reps",
            &reps,
        ]);
        if let Some(c) = flag(args, "--commit-size") {
            cmd.args(["--commit-size", &c]);
        }
        let o = supervise(cmd, name, Duration::from_secs(limit));
        eprintln!(
            "  {}  {}  [{} {}{}]",
            o.status,
            summary(&o.last),
            verdict(&o),
            target_text(target(name)),
            if gate.iter().any(|g| g == name) {
                ", gated"
            } else {
                ""
            }
        );
        let failed_load = *name == "load" && o.status != "ok";
        outcomes.push(o);
        if failed_load {
            eprintln!("the load did not complete: the other scenarios are skipped");
            break;
        }
    }
    let table = render(&scale, &url, &outcomes);
    println!("{table}");
    if let Some(file) = flag(args, "--json") {
        let doc = json!({
            "scale": scale,
            "seed": seed(args),
            "database": if url.starts_with("sqlite:") { "sqlite" } else { "postgres" },
            "timeout_s": timeout,
            "machine": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0) },
            "steps": outcomes.iter().map(|o| {
                let mut v = o.last.clone();
                if !v.is_object() { v = json!({}); }
                v["name"] = json!(o.name);
                v["description"] = json!(STEPS.iter().find(|s| s.0 == o.name).map(|s| s.1));
                v["status"] = json!(o.status);
                v["seconds"] = json!((o.secs * 100.0).round() / 100.0);
                v["target"] = json!(target_text(target(&o.name)));
                v["verdict"] = json!(verdict(o));
                v["gated"] = json!(gate.iter().any(|g| *g == o.name));
                v
            }).collect::<Vec<_>>(),
        });
        std::fs::write(&file, serde_json::to_string_pretty(&doc).unwrap())
            .map_err(|e| format!("cannot write {file}: {e}"))?;
        eprintln!("results written to {file}");
    }
    let failed: Vec<&str> = outcomes
        .iter()
        .filter(|o| gate.iter().any(|g| *g == o.name) && verdict(o) == "fail")
        .map(|o| o.name.as_str())
        .collect();
    // a gated scenario that did not run (the load failed) fails too
    let missing: Vec<&str> = gate
        .iter()
        .filter(|g| steps_run(&outcomes, g))
        .map(String::as_str)
        .collect();
    if !failed.is_empty() || !missing.is_empty() {
        return Err(format!(
            "gated scenarios failed: {}",
            failed
                .iter()
                .chain(missing.iter())
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

/// A gated scenario that was asked for but never ran.
fn steps_run(outcomes: &[Outcome], g: &str) -> bool {
    !outcomes.iter().any(|o| o.name == g)
        && outcomes
            .iter()
            .any(|o| o.name == "load" && o.status != "ok")
}

/// Run a step in a child process, killed after `limit`.
fn supervise(mut cmd: Command, name: &str, limit: Duration) -> Outcome {
    let started = Instant::now();
    let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn() {
        Ok(c) => c,
        Err(e) => {
            return Outcome {
                name: name.into(),
                status: "error".into(),
                last: json!({ "error": e.to_string() }),
                secs: 0.0,
            }
        }
    };
    let last: Arc<Mutex<Option<J>>> = Arc::new(Mutex::new(None));
    let stdout = child.stdout.take().unwrap();
    let keep = last.clone();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if let Ok(v) = serde_json::from_str::<J>(&line) {
                let mut last = keep.lock().unwrap();
                match (v.get("progress"), last.as_mut()) {
                    (Some(p), Some(l)) => l["progress"] = p.clone(),
                    (Some(p), None) => *last = Some(json!({ "progress": p })),
                    (None, _) => *last = Some(v),
                }
            }
        }
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                break if s.success() {
                    "ok".to_string()
                } else {
                    "error".to_string()
                }
            }
            Ok(None) if started.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                // the changed copy of the project a killed deploy step could not remove
                let prefix = format!("quadra-bench-{}-", child.id());
                for e in std::fs::read_dir(std::env::temp_dir())
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    if e.file_name().to_string_lossy().starts_with(&prefix) {
                        let _ = std::fs::remove_dir_all(e.path());
                    }
                }
                break "timeout".to_string();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => break format!("error: {e}"),
        }
    };
    let _ = reader.join();
    let mut last = last.lock().unwrap().clone().unwrap_or(json!({}));
    if status == "timeout" {
        last["timeout_s"] = json!(limit.as_secs());
    }
    Outcome {
        name: name.into(),
        status,
        last,
        secs: started.elapsed().as_secs_f64(),
    }
}

fn fmt_ms(v: &J) -> String {
    match v.as_f64() {
        None => "—".into(),
        Some(x) if x >= 10_000.0 => format!("{:.1} s", x / 1000.0),
        Some(x) if x >= 100.0 => format!("{x:.0} ms"),
        Some(x) => format!("{x:.1} ms"),
    }
}

fn summary(v: &J) -> String {
    let mut parts = Vec::new();
    if let Some(e) = v["error"].as_str() {
        return format!("error: {}", e.lines().next().unwrap_or(""));
    }
    if !v["cold_ms"].is_null() {
        parts.push(format!("cold {}", fmt_ms(&v["cold_ms"])));
    }
    if !v["p50_ms"].is_null() {
        parts.push(format!(
            "p50 {} p95 {}",
            fmt_ms(&v["p50_ms"]),
            fmt_ms(&v["p95_ms"])
        ));
    }
    if let Some(n) = v["note"].as_str() {
        parts.push(n.to_string());
    }
    parts.join(", ")
}

fn render(scale: &str, url: &str, outcomes: &[Outcome]) -> String {
    let db = if url.starts_with("sqlite:") {
        "SQLite"
    } else {
        "PostgreSQL"
    };
    let mut s = format!("\n{db}, scale {scale}\n\n");
    s.push_str(&format!(
        "{:<20} {:>10} {:>10} {:>10} {:>9} {:>9}  {:<5} {:<16}  {}\n",
        "scenario", "cold", "p50", "p95", "peak MB", "SQL cold", "", "target", "notes"
    ));
    for o in outcomes {
        let v = &o.last;
        let cold = match o.status.as_str() {
            "timeout" if v["cold_ms"].is_null() => format!("> {} s", v["timeout_s"]),
            "ok" | "timeout" => fmt_ms(&v["cold_ms"]),
            _ => "error".into(),
        };
        let warm = |k: &str| {
            if o.status == "timeout" && v[k].is_null() && !v["cold_ms"].is_null() {
                format!("> {} s", v["timeout_s"])
            } else {
                fmt_ms(&v[k])
            }
        };
        let note = if o.status.starts_with("error") {
            v["error"]
                .as_str()
                .unwrap_or("failed")
                .lines()
                .next()
                .unwrap_or("")
                .to_string()
        } else if o.status == "timeout" && v["cold_ms"].is_null() {
            let p = &v["progress"];
            format!(
                "killed after {} records loaded, {} statements",
                p["records_loaded"], p["statements"]
            )
        } else {
            let db = v["cold_db_ms"]
                .as_f64()
                .map(|d| format!(" [database {d} ms of the cold call]"))
                .unwrap_or_default();
            format!("{}{db}", v["note"].as_str().unwrap_or(""))
        };
        let rss = v["rss_mb"]
            .as_f64()
            .or(v["progress"]["rss_mb"].as_f64())
            .map(|x| format!("{x:.0}"))
            .unwrap_or("—".into());
        let stmts = v["cold_statements"]
            .as_u64()
            .map(|x| x.to_string())
            .unwrap_or("—".into());
        s.push_str(&format!(
            "{:<20} {:>10} {:>10} {:>10} {:>9} {:>9}  {:<5} {:<16}  {}\n",
            o.name,
            cold,
            warm("p50_ms"),
            warm("p95_ms"),
            rss,
            stmts,
            verdict(o),
            target_text(target(&o.name)),
            note
        ));
    }
    s
}

/// Start from an empty database: the SQLite file, or the PostgreSQL schema of the URL.
fn wipe(url: &str) -> Result<(), String> {
    if let Some(path) = url.strip_prefix("sqlite:") {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let p = format!("{path}{suffix}");
            if Path::new(&p).exists() {
                std::fs::remove_file(&p).map_err(|e| format!("cannot remove {p}: {e}"))?;
            }
        }
        return Ok(());
    }
    let Some((base, schema)) = url.split_once("#schema=") else {
        return Err("on PostgreSQL the benchmark loads into a schema of its own, dropped and created again: add #schema=name to the URL".into());
    };
    let mut c = Conn::open(base)?;
    c.batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
}

/// Bytes of the database (SQLite file and its WAL; the tables of the PostgreSQL schema).
fn db_bytes(url: &str, conn: &mut Conn) -> Option<u64> {
    if let Some(path) = url.strip_prefix("sqlite:") {
        let len = |p: &str| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        return Some(len(path) + len(&format!("{path}-wal")));
    }
    let r = conn
        .query("SELECT CAST(SUM(pg_total_relation_size(c.oid)) AS TEXT) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = current_schema() AND c.relkind = 'r'", &[])
        .ok()?;
    r.first()?.first()?.as_deref()?.parse().ok()
}

// ---- the steps (in the child) ------------------------------------------------------------

fn now() -> chrono::NaiveDateTime {
    let t = chrono::Local::now().naive_local();
    t - chrono::Duration::nanoseconds(t.and_utc().timestamp_subsec_nanos() as i64)
}

fn step(name: &str, args: &[String]) -> Result<(), String> {
    let path = path_arg(args);
    let url = flag(args, "--url").ok_or("--url is required")?;
    let scale = flag(args, "--scale").unwrap_or_else(|| "small".into());
    let reps: usize = flag(args, "--reps")
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    // how far a step got, should it be killed by the timeout
    let started = Instant::now();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(5));
        let (s, r) = counters();
        println!(
            "{}",
            json!({ "progress": { "seconds": started.elapsed().as_secs(), "statements": s, "records_loaded": r, "rss_mb": peak_rss_mb().round() } })
        );
        let _ = std::io::stdout().flush();
    });
    // the limits a server applies to its requests: the interpreter counts its steps against
    // the deadline as it does in `quadra serve` (the deadline itself is never reached here)
    let _limits = quadra_runtime::limits::begin(quadra_runtime::limits::Limits {
        deadline: Some(Instant::now() + Duration::from_secs(86_400)),
        ..Default::default()
    });
    let mut m = Meter::new();
    let r = match name {
        "load" => load(
            &path,
            &url,
            &scale,
            seed(args),
            flag(args, "--commit-size").and_then(|v| v.parse().ok()),
            &mut m,
        ),
        n if n.starts_with("deploy_") => deploy(&path, &url, n, &mut m),
        "verify" => verify(&path, &url, &mut m),
        n if n.starts_with("sql_") => sql(&url, n, reps, &mut m),
        n => api(&path, &url, n, reps, &mut m),
    };
    match r {
        Ok(()) => {
            m.emit(true);
            Ok(())
        }
        Err(e) => {
            m.set("error", json!(e));
            m.emit(true);
            Err(e)
        }
    }
}

fn checked(path: &Path) -> Result<(PathBuf, quadra_analysis::Checked), String> {
    let root = quadra_sema::find_project_root(path)
        .ok_or_else(|| format!("no quadra.toml in {} or its parents", path.display()))?;
    let project = quadra_sema::Project::load(&root).map_err(|e| e.to_string())?;
    let checked = quadra_analysis::check(&project);
    let Some(model) = &checked.model else {
        return Err(format!("{}: the project has errors", root.display()));
    };
    for e in ["Region", "Customer", "Order", "OrderLine", "Shipment"] {
        if model.entity(e).is_none() {
            return Err(format!(
                "{}: not the stress model (no `{e}`): run the benchmark on examples/stress",
                root.display()
            ));
        }
    }
    Ok((root, checked))
}

fn load(
    path: &Path,
    url: &str,
    scale: &str,
    seed: u64,
    per_commit: Option<usize>,
    m: &mut Meter,
) -> Result<(), String> {
    let (_, checked) = checked(path)?;
    let model = checked.model.as_ref().unwrap();
    let report = &checked.report;
    let t = Instant::now();
    quadra_store::ops::init(url, model, report, now())?;
    let mut db = quadra_store::Database::open(url, model, report)?;
    let mut import = quadra_store::bulk::Import::new(
        &mut db,
        model,
        report,
        now(),
        &format!("stress {scale} seed {seed}"),
    )?;
    if let Some(n) = per_commit {
        import.records_per_commit = n;
    }
    let g = stress::generate(scale, seed, &mut stress::ToImport::new(&mut import))?;
    let r = import.run(db, &mut |l| eprintln!("{l}"))?;
    let total = t.elapsed().as_secs_f64();
    let mut conn = Conn::open(url)?;
    m.cold = Some((total * 1e5).round() / 100.0);
    m.set("rows", json!(g.total()));
    m.set(
        "entities",
        json!(g
            .counts
            .iter()
            .map(|(e, n)| (e.clone(), json!(n)))
            .collect::<serde_json::Map<_, _>>()),
    );
    m.set("history_rows", json!(r.history));
    m.set("commits", json!(r.commits.len()));
    m.set(
        "phases_s",
        json!(r
            .phases
            .iter()
            .map(|(p, d)| (p.clone(), json!((d.as_secs_f64() * 100.0).round() / 100.0)))
            .collect::<serde_json::Map<_, _>>()),
    );
    m.set("rows_per_s", json!((g.total() as f64 / total).round()));
    let bytes = db_bytes(url, &mut conn);
    m.set(
        "db_mb",
        json!(bytes.map(|b| (b as f64 / 1_048_576.0).round())),
    );
    m.set(
        "note",
        json!(format!(
            "{} rows, {:.0} rows/s, {} commits, db {} MB",
            g.total(),
            g.total() as f64 / total,
            r.commits.len(),
            bytes.map(|b| b / 1_048_576).unwrap_or(0)
        )),
    );
    Ok(())
}

/// One value of a query on the database (the targets of the scenarios are picked by SQL on
/// the projections: not timed).
fn one(conn: &mut Conn, sql: &str, params: &[Sql]) -> Result<String, String> {
    conn.query(sql, params)?
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next().flatten())
        .ok_or_else(|| format!("no row for: {sql}"))
}

fn user(conn: &mut Conn, login: &str) -> Result<u64, String> {
    one(
        conn,
        "SELECT CAST(\"id\" AS TEXT) FROM \"user\" WHERE \"login\" = ?text",
        &[Sql::Text(login.into())],
    )?
    .parse()
    .map_err(|_| "bad id".to_string())
}

fn head(conn: &mut Conn) -> Result<u64, String> {
    quadra_store::source::head(conn)
}

fn api(path: &Path, url: &str, name: &str, reps: usize, m: &mut Meter) -> Result<(), String> {
    let (root, _) = checked(path)?;
    let mut conn = Conn::open(url)?;
    let admin = user(&mut conn, stress::ADMIN)?;
    let sales = user(&mut conn, stress::SALESPERSON)?;
    let logistics = user(&mut conn, "logistics01")?;
    let manager = user(&mut conn, "manager01")?;
    let controller = user(&mut conn, stress::CONTROLLER)?;
    let r01 = one(
        &mut conn,
        "SELECT CAST(\"id\" AS TEXT) FROM \"region\" WHERE \"code\" = 'R01'",
        &[],
    )?;
    let mut local = quadra_server::Local::open(&root, url, 256)?;
    let grid = |w: Option<&str>, order: Option<J>| {
        let mut q = json!({ "entity": "Shipment", "limit": 500, "offset": 0 });
        if let Some(w) = w {
            q["where"] = json!(w);
        }
        if let Some(o) = order {
            q["order_by"] = o;
        }
        q
    };
    let query = |local: &mut quadra_server::Local,
                 m: &mut Meter,
                 cold: bool,
                 user: u64,
                 q: &J|
     -> Result<(), String> {
        let (status, body) = local.query(user, q);
        if status != 200 {
            return Err(format!("{status}: {body}"));
        }
        if cold {
            let (hits, misses, len, bytes) = local.cache();
            m.set(
                "rows",
                json!(body["rows"].as_array().map_or(0, |r| r.len())),
            );
            m.set("total", body["total"].clone());
            m.set("loaded_records", json!(misses));
            m.set(
                "note",
                json!(format!(
                    "{} rows of {}, {misses} records loaded ({} cached, {} MB), {hits} cache hits",
                    body["rows"].as_array().map_or(0, |r| r.len()),
                    body["total"],
                    len,
                    bytes >> 20
                )),
            );
        }
        Ok(())
    };
    let record = |local: &mut quadra_server::Local,
                  m: &mut Meter,
                  cold: bool,
                  entity: &str,
                  id: u64|
     -> Result<(), String> {
        let (status, body) = local.record(admin, entity, id);
        if status != 200 {
            return Err(format!("{status}: {body}"));
        }
        if cold {
            let (_, misses, _, _) = local.cache();
            m.set("loaded_records", json!(misses));
            m.set(
                "note",
                json!(format!("{entity}#{id}, {misses} records loaded")),
            );
        }
        Ok(())
    };
    let id = |conn: &mut Conn, sql: &str| -> Result<u64, String> {
        one(conn, sql, &[])?
            .parse()
            .map_err(|_| "bad id".to_string())
    };
    // who reads in the scenarios under row permissions (the new region's manager for move_read)
    let as_user = match name {
        "grid_sales" | "agg_sales" | "search_sales" | "ref_picker_sales" | "group_sales"
        | "fts_sales" | "kanban_sales" | "app_open_sales" => Some(stress::SALESPERSON),
        "grid_logistics" => Some("logistics01"),
        "grid_manager" => Some("manager01"),
        "grid_deny" => Some(stress::CONTROLLER),
        "move_read" => Some("manager01/manager02"),
        _ => None,
    };
    let r = if name.starts_with("app_") {
        app_step(path, name, reps, m, &mut conn, &mut local, admin, sales)
    } else {
        api_step(
            name, reps, m, &mut conn, &mut local, admin, sales, logistics, manager, controller,
            &r01, &grid, &query, &record, &id,
        )
    };
    if let Some(login) = as_user {
        m.set("user", json!(login));
        let note = m
            .extra
            .get("note")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        m.set("note", json!(format!("as {login}: {note}")));
    }
    r
}

/// The Sales cockpit of examples/stress (`apps/sales-cockpit.json`), shown as the web client
/// shows it (`Local::open_app`: the requests of `quadra_server::open_requests`).
#[allow(clippy::too_many_arguments)]
fn app_step(
    path: &Path,
    name: &str,
    reps: usize,
    m: &mut Meter,
    conn: &mut Conn,
    local: &mut quadra_server::Local,
    admin: u64,
    sales: u64,
) -> Result<(), String> {
    let file = path.join("apps/sales-cockpit.json");
    let doc: J = serde_json::from_str(
        &std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?,
    )
    .map_err(|e| format!("{}: {e}", file.display()))?;
    let cfg = doc["views"][0]["config"].clone();
    let ok = |r: &J| -> Result<(), String> {
        let errors: Vec<String> = r["errors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e.as_str().map(str::to_string))
            .take(3)
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!("components in error: {}", errors.join("; ")))
        }
    };
    let note = |r: &J, extra: &str| {
        format!(
            "{} components, {} requests, {} ms in formulas (the rest: queries, the board){extra}",
            r["results"].as_object().map_or(0, |o| o.len()),
            r["requests"],
            r["ms_engine"]
        )
    };
    let regions: Vec<u64> = conn
        .query(
            "SELECT CAST(\"id\" AS TEXT) FROM \"region\" ORDER BY \"code\"",
            &[],
        )?
        .into_iter()
        .filter_map(|r| r[0].as_deref().and_then(|v| v.parse().ok()))
        .collect();
    match name {
        "app_open" | "app_open_sales" => {
            let user = if name == "app_open" { admin } else { sales };
            m.time(reps, |m, cold| {
                // every repetition with its formulas cold (the record cache warms)
                local.clear_sheets();
                let r = local.open_app(user, &cfg, &json!({}));
                ok(&r)?;
                if cold {
                    m.set("rows", r["requests"].clone());
                    m.set("note", json!(note(&r, "")));
                }
                Ok(())
            })
        }
        "app_filter_change" => {
            let r = local.open_app(admin, &cfg, &json!({}));
            ok(&r)?;
            let checks = local.sheet_checks();
            let mut k = 0;
            m.time(reps, |m, cold| {
                k += 1;
                let region = regions[k % regions.len().max(1)];
                let r = local.refresh_app(admin, &cfg, &json!({ "region": [region] }));
                ok(&r)?;
                if local.sheet_checks() != checks {
                    return Err(format!(
                        "a filter's value changed and formulas were checked again ({} checks)",
                        local.sheet_checks() - checks
                    ));
                }
                if cold {
                    m.set("rows", r["requests"].clone());
                    m.set("note", json!(note(&r, ", nothing checked again")));
                }
                Ok(())
            })
        }
        _ => {
            // the board's draft orders the model lets confirm, one per click
            let rows = conn.query(
                &format!("SELECT CAST(o.\"id\" AS TEXT) FROM \"order\" o JOIN \"customer\" c ON c.\"id\" = o.\"customer_id\" JOIN \"region\" r ON r.\"id\" = c.\"region_id\" WHERE o.\"state\" = 'Draft' AND o.\"line_count\" > 0 AND c.\"active\" AND r.\"active\" AND NOT r.\"locked\" ORDER BY o.\"total\", o.\"id\" LIMIT {}", reps + 1),
                &[],
            )?;
            let drafts: Vec<u64> = rows
                .iter()
                .filter_map(|r| r[0].as_deref().and_then(|v| v.parse().ok()))
                .collect();
            if drafts.len() < reps + 1 {
                return Err("not enough draft orders to confirm".into());
            }
            let r = local.open_app(admin, &cfg, &json!({ "order": drafts[0] }));
            ok(&r)?;
            let mut n = 0usize;
            m.time(reps, |m, cold| {
                let o = drafts[n];
                n += 1;
                // the button reads its record (its base, what may run now), runs the transition
                let (status, rec) = local.record(admin, "Order", o);
                if status != 200 {
                    return Err(format!("{status}: {rec}"));
                }
                let body = json!({ "base": rec["base"], "ops": [{ "op": "action", "entity": "Order", "id": o, "action": "confirm" }] });
                let (status, t) = local.tx(admin, &body, &format!("bench-app-{}-{n}", std::process::id()));
                if status != 200 {
                    return Err(format!("{status}: {t}"));
                }
                // the commit reaches the open components: they evaluate again
                let r = local.refresh_app(admin, &cfg, &json!({ "order": o }));
                ok(&r)?;
                if cold {
                    m.set("rows", r["requests"].clone());
                    m.set("note", json!(note(&r, &format!(", after Order#{o} Draft -> Confirmed"))));
                }
                Ok(())
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn api_step(
    name: &str,
    reps: usize,
    m: &mut Meter,
    mut conn: &mut Conn,
    mut local: &mut quadra_server::Local,
    admin: u64,
    sales: u64,
    logistics: u64,
    manager: u64,
    controller: u64,
    r01: &str,
    grid: &dyn Fn(Option<&str>, Option<J>) -> J,
    query: &dyn Fn(&mut quadra_server::Local, &mut Meter, bool, u64, &J) -> Result<(), String>,
    record: &dyn Fn(&mut quadra_server::Local, &mut Meter, bool, &str, u64) -> Result<(), String>,
    id: &dyn Fn(&mut Conn, &str) -> Result<u64, String>,
) -> Result<(), String> {
    match name {
        "grid" => m.time(reps, |m, cold| query(&mut local, m, cold, admin, &grid(None, None))),
        "grid_where1" => m.time(reps, |m, cold| query(&mut local, m, cold, admin, &grid(Some("line.qty >= 15 pcs"), None))),
        "grid_where3" => {
            let code = one(&mut conn, &format!("SELECT \"code\" FROM \"customer\" WHERE \"region_id\" = {r01} ORDER BY \"id\" LIMIT 1 OFFSET 5"), &[])?;
            let w = format!("line.order.customer.code == \"{code}\"");
            m.time(reps, |m, cold| query(&mut local, m, cold, admin, &grid(Some(&w), None)))
        }
        "grid_sorted" => m.time(reps, |m, cold| query(&mut local, m, cold, admin, &grid(None, Some(json!([{ "field": "date", "desc": true }]))))),
        "grid_sales" => m.time(reps, |m, cold| query(&mut local, m, cold, sales, &grid(None, None))),
        "grid_logistics" => m.time(reps, |m, cold| query(&mut local, m, cold, logistics, &grid(None, None))),
        "grid_manager" => m.time(reps, |m, cold| query(&mut local, m, cold, manager, &json!({ "entity": "Order", "limit": 500 }))),
        "grid_deny" => m.time(reps, |m, cold| query(&mut local, m, cold, controller, &json!({ "entity": "Order", "limit": 500 }))),
        "agg_sales" => {
            let q = json!({ "entity": "Shipment", "limit": 50, "aggregates": { "qty": "sum", "date": "max", "tracking": "count" } });
            m.time(reps, |m, cold| {
                query(&mut local, m, cold, sales, &q)?;
                if cold {
                    let (_, body) = local.query(sales, &q);
                    m.set("aggregates", body["aggregates"].clone());
                }
                Ok(())
            })
        }
        "ref_picker_sales" => m.time(reps, |m, cold| query(&mut local, m, cold, sales, &json!({ "entity": "Customer", "fields": [], "limit": 1000 }))),
        "search_sales" => m.time(reps, |m, cold| {
            let (status, body) = local.search(sales, "Customer", "rossi", 50);
            if status != 200 {
                return Err(format!("{status}: {body}"));
            }
            if cold {
                let (_, misses, _, _) = local.cache();
                let n = body["rows"].as_array().map_or(0, |r| r.len());
                m.set("rows", json!(n));
                m.set("note", json!(format!("{n} customers found, {misses} records loaded")));
            }
            Ok(())
        }),
        "grid_lines" => m.time(reps, |m, cold| query(&mut local, m, cold, admin, &json!({ "entity": "OrderLine", "limit": 500 }))),
        "group_region_month" | "group_sales" | "pivot_category" | "pivot_lines" => {
            let (user, q) = match name {
                "pivot_lines" => (admin, json!({ "entity": "OrderLine", "group_by": ["product.category", "order.state"], "pivot": true, "aggregates": { "amount": "sum", "qty": "sum" }, "limit": 100 })),
                "group_region_month" => (admin, json!({ "entity": "Order", "where": "state in {Confirmed, Shipped}", "group_by": ["customer.region", { "field": "date", "bucket": "month" }], "aggregates": { "total": "sum" }, "limit": 100 })),
                "group_sales" => (sales, json!({ "entity": "Order", "group_by": ["customer", "state"], "aggregates": { "total": "sum" }, "limit": 100 })),
                _ => (admin, json!({ "entity": "Product", "group_by": ["category", "active"], "pivot": true, "aggregates": { "units_sold": "sum", "margin": "sum" }, "limit": 100 })),
            };
            m.time(reps, |m, cold| {
                let (status, body) = local.query(user, &q);
                if status != 200 {
                    return Err(format!("{status}: {body}"));
                }
                if body["sql"] != json!(true) {
                    return Err(format!("the groups were not computed in SQL: {}", body.to_string().chars().take(300).collect::<String>()));
                }
                if cold {
                    let n = body["groups"].as_array().map_or(0, |g| g.len());
                    let rows: u64 = body["groups"].as_array().map_or(0, |g| g.iter().map(|x| x["count"].as_u64().unwrap_or(0)).sum());
                    m.set("rows", json!(n));
                    m.set("total", body["groups_total"].clone());
                    let cols = body["columns"].as_array().map(|c| format!(" × {} columns", c.len())).unwrap_or_default();
                    m.set("note", json!(format!("{n} of {} groups{cols}, {rows} rows in them", body["groups_total"])));
                }
                Ok(())
            })
        }
        "fts_customer" | "fts_sales" => {
            let (user, text) = if name == "fts_customer" { (admin, "rossi logis") } else { (sales, "tyrell figli") };
            let q = json!({ "entity": "Customer", "fields": ["code", "name", "region"], "search": text, "limit": 50 });
            m.time(reps, |m, cold| query(&mut local, m, cold, user, &q))
        }
        "sheet_eval" | "sheet_recalc" => {
            // a sheet of figures per region: the codes in column A, ten formulas reading the
            // database to the right of each, a ratio of two of them, and a spill of 1000 orders
            let mut codes: Vec<String> = conn.query("SELECT \"code\" FROM \"region\" ORDER BY \"code\"", &[])?.into_iter().filter_map(|r| r[0].clone()).collect();
            while codes.len() < 20 {
                codes.push(format!("X{:02}", codes.len()));
            }
            let figures = [
                "Region.find(r => r.code == $A)?.revenue",
                "Region.find(r => r.code == $A)?.open_total",
                "Region.find(r => r.code == $A)?.customer_count",
                "Region.find(r => r.code == $A)?.active_customers",
                "Region.find(r => r.code == $A)?.open_orders",
                "Customer.count(c => c.region.code == $A and c.active)",
                "Customer.where(c => c.region.code == $A).sum(c => c.revenue)",
                "Region.find(r => r.code == $A)?.to_ship",
                "Campaign.count(c => c.region.code == $A)",
                "Customer.where(c => c.region.code == $A).max(c => c.credit_limit)",
            ];
            let mut cells = serde_json::Map::new();
            cells.insert("A1".into(), json!({ "v": "Region" }));
            for (k, code) in codes.iter().take(20).enumerate() {
                let row = k + 2;
                cells.insert(format!("A{row}"), json!({ "v": code }));
                for (c, f) in figures.iter().enumerate() {
                    let col = (b'B' + c as u8) as char;
                    let f = f.replace("$A", &format!("A{row}"));
                    cells.insert(format!("{col}{row}"), json!({ "v": format!("={f}") }));
                }
                cells.insert(format!("L{row}"), json!({ "v": format!("=(B{row} / D{row}).round() if (D{row} ?? 0) > 0 else none") }));
            }
            cells.insert("N1".into(), json!({ "v": "=Order.where(o => o.state is Confirmed).sort_by(o => o.number).first(1000).map(o => (o.number, o.date, o.customer.code, o.total))" }));
            let book = |a2: &str| {
                let mut c = cells.clone();
                c.insert("A2".into(), json!({ "v": a2 }));
                json!({ "sheets": [{ "id": "s1", "name": "Regions", "cells": c }] })
            };
            let check = |body: &J, m: &mut Meter, cold: bool, partial: bool| -> Result<(), String> {
                let cells = body["cells"].as_object().ok_or("no cells")?;
                let errors: Vec<String> = cells.iter().filter(|(_, c)| c["error"].is_string()).map(|(k, c)| format!("{k}: {}", c["error"])).take(3).collect();
                if !errors.is_empty() {
                    return Err(format!("formulas in error: {}", errors.join("; ")));
                }
                if !partial && cells["s1!N1"]["spill"]["rows"].as_u64().unwrap_or(0) == 0 {
                    return Err("the spill is empty".into());
                }
                if cold {
                    let db = cells.values().filter(|c| c["db"] == json!(true)).count();
                    m.set("rows", json!(cells.len()));
                    m.set("note", json!(format!("{} formulas answered ({db} read the database), spill of {} rows, {} ms in the engine", cells.len(), cells.get("s1!N1").map_or(0, |c| c["spill"]["rows"].as_u64().unwrap_or(0)), body["ms"])));
                }
                Ok(())
            };
            if name == "sheet_eval" {
                m.time(reps, |m, cold| {
                    // every repetition from cold workbooks and formulas (the records' cache warms)
                    local.clear_sheets();
                    let (status, body) = local.eval(admin, &book(&codes[0]));
                    if status != 200 {
                        return Err(format!("{status}: {body}"));
                    }
                    check(&body, m, cold, false)
                })
            } else {
                let (status, body) = local.eval(admin, &book(&codes[0]));
                if status != 200 {
                    return Err(format!("{status}: {body}"));
                }
                let mut k = 0;
                m.time(reps, |m, cold| {
                    k += 1;
                    let code = codes[k % codes.len()].clone();
                    let mut b = book(&code);
                    b["changed"] = json!(["s1!A2"]);
                    let (status, body) = local.eval(admin, &b);
                    if status != 200 {
                        return Err(format!("{status}: {body}"));
                    }
                    check(&body, m, cold, true)?;
                    if cold {
                        let n = body["cells"].as_object().map_or(0, |c| c.len());
                        m.set("note", json!(format!("A2 = {code}: {n} formulas answered, {} ms in the engine", body["ms"])));
                    }
                    Ok(())
                })
            }
        }
        "kanban_orders" | "kanban_sales" => {
            // the board as the web client asks for it: the columns' counts and totals in one
            // group query, then the first page of each column (its cards' fields), counts not
            // asked again
            let user = if name == "kanban_orders" { admin } else { sales };
            let counts = json!({ "entity": "Order", "group_by": ["state"], "aggregates": { "total": "sum" }, "limit": 100 });
            let column = |state: &str| json!({ "entity": "Order", "fields": ["number", "customer", "total", "date", "priority"], "group_filter": [{ "field": "state", "value": state }], "order_by": [{ "field": "date", "desc": true }], "limit": 50, "count": false });
            m.time(reps, |m, cold| {
                let (status, groups) = local.query(user, &counts);
                if status != 200 {
                    return Err(format!("{status}: {groups}"));
                }
                if groups["sql"] != json!(true) {
                    return Err("the counts of the columns were not computed in SQL".into());
                }
                let mut cards = 0;
                for state in stress::STATES {
                    let (status, body) = local.query(user, &column(state));
                    if status != 200 {
                        return Err(format!("{status}: {body}"));
                    }
                    cards += body["rows"].as_array().map_or(0, |r| r.len());
                }
                if cold {
                    let (_, misses, _, _) = local.cache();
                    let counts: Vec<String> = groups["groups"].as_array().into_iter().flatten().map(|g| format!("{} {}", g["keys"][0]["d"].as_str().unwrap_or(""), g["count"])).collect();
                    m.set("rows", json!(cards));
                    m.set("note", json!(format!("{} columns ({}), {cards} cards, {misses} records loaded", stress::STATES.len(), counts.join(", "))));
                }
                Ok(())
            })
        }
        "kanban_move" => {
            // draft orders whose confirmation the model accepts (lines, an active customer in an
            // open region), the smallest first (their customer's credit limit holds): one per drag
            let rows = conn.query(
                &format!("SELECT CAST(o.\"id\" AS TEXT) FROM \"order\" o JOIN \"customer\" c ON c.\"id\" = o.\"customer_id\" JOIN \"region\" r ON r.\"id\" = c.\"region_id\" WHERE o.\"state\" = 'Draft' AND o.\"line_count\" > 0 AND c.\"active\" AND r.\"active\" AND NOT r.\"locked\" ORDER BY o.\"total\", o.\"id\" LIMIT {}", reps + 1),
                &[],
            )?;
            let drafts: Vec<u64> = rows.iter().filter_map(|r| r[0].as_deref().and_then(|v| v.parse().ok())).collect();
            if drafts.len() < reps + 1 {
                return Err("not enough draft orders to move".into());
            }
            let mut n = 0usize;
            let mut base = head(&mut conn)?;
            m.time(reps, |m, cold| {
                let o = drafts[n];
                n += 1;
                let body = json!({ "base": base, "ops": [{ "op": "action", "entity": "Order", "id": o, "action": "confirm" }] });
                let (status, r) = local.tx(admin, &body, &format!("bench-kanban-{}-{n}", std::process::id()));
                if status != 200 {
                    return Err(format!("{status}: {r}"));
                }
                base = r["base"].as_u64().unwrap_or(base + 1);
                if cold {
                    let (_, misses, _, _) = local.cache();
                    m.set("loaded_records", json!(misses));
                    m.set("note", json!(format!("Order#{o} Draft -> Confirmed, {misses} records loaded by the first move")));
                }
                Ok(())
            })
        }
        "agg_region" => m.time(reps, |m, cold| query(&mut local, m, cold, admin, &json!({ "entity": "Region", "fields": ["code", "revenue", "open_total", "customer_count"], "limit": 1000 }))),
        "agg_top_customers" => {
            let q = json!({ "entity": "Customer", "fields": ["code", "name", "revenue"], "order_by": [{ "field": "revenue", "desc": true }], "limit": 10 });
            m.time(reps, |m, cold| query(&mut local, m, cold, admin, &q))
        }
        "search_prefix" => {
            let q = json!({ "entity": "Customer", "fields": ["code", "name"], "where": "name.starts_with(\"Rossi\")", "limit": 50 });
            m.time(reps, |m, cold| query(&mut local, m, cold, admin, &q))
        }
        "ref_picker" => m.time(reps, |m, cold| query(&mut local, m, cold, admin, &json!({ "entity": "Customer", "fields": [], "limit": 1000 }))),
        "scroll" => {
            // pages 1 to 49 as the client scrolls (not timed), then page 50
            let mut q = json!({ "entity": "Shipment", "where": "carrier is Air", "order_by": [{ "field": "date", "desc": true }], "limit": 100 });
            let mut page = 1;
            while page < 50 {
                let (status, body) = local.query(admin, &q);
                if status != 200 {
                    return Err(format!("{status}: {body}"));
                }
                // a small world has fewer pages: the last one
                let Some(next) = body["next"].as_str() else { break };
                q["after"] = json!(next);
                q["count"] = json!(false);
                page += 1;
            }
            m.set("page", json!(page));
            m.time(reps, |m, cold| query(&mut local, m, cold, admin, &q))
        }
        "query_action" => {
            let r: u64 = r01.parse().map_err(|_| "bad id")?;
            let mut n = 0u64;
            let mut base = head(&mut conn)?;
            m.time(reps, |m, cold| {
                n += 1;
                let body = json!({ "base": base, "ops": [{ "op": "action", "entity": "Region", "id": r, "action": "review", "args": { "since": "2026-01-01" } }] });
                let (status, b) = local.tx(admin, &body, &format!("bench-review-{}-{n}", std::process::id()));
                if status != 200 {
                    return Err(format!("{status}: {b}"));
                }
                base = b["base"].as_u64().unwrap_or(base + 1);
                if cold {
                    let (_, misses, _, _) = local.cache();
                    m.set("loaded_records", json!(misses));
                    m.set("note", json!(format!("Region#{r}.review, {misses} records loaded")));
                }
                Ok(())
            })
        }
        "query_check" => {
            let r: u64 = r01.parse().map_err(|_| "bad id")?;
            let mut n = 0u64;
            m.time(reps, |m, cold| {
                n += 1;
                let body = json!({ "ops": [{ "op": "create", "entity": "Campaign", "values": { "name": "Spring", "region": r, "max_drafts": 0 } }] });
                let (status, b) = local.tx(admin, &body, &format!("bench-campaign-{}-{n}", std::process::id()));
                let refused = status != 200 && b.to_string().contains("Too many draft orders");
                if !refused {
                    return Err(format!("expected the check to refuse the campaign: {status}: {b}"));
                }
                if cold {
                    m.set("note", json!("Campaign refused by its check (Order.count over the region)"));
                }
                Ok(())
            })
        }
        "query_formula" => {
            let q = json!({ "entity": "Region", "fields": ["code"], "columns": { "orders_2026": "Order.count(o => o.customer.region == self and o.date >= 2026-01-01)" }, "limit": 100 });
            m.time(reps, |m, cold| query(&mut local, m, cold, admin, &q))
        }
        "record_order" => {
            let o = id(&mut conn, "SELECT CAST(\"id\" AS TEXT) FROM \"order\" WHERE \"state\" = 'Confirmed' ORDER BY \"id\" LIMIT 1 OFFSET 10")?;
            m.time(reps, |m, cold| record(&mut local, m, cold, "Order", o))
        }
        "record_customer" => {
            let c = id(&mut conn, &format!("SELECT CAST(\"id\" AS TEXT) FROM \"customer\" WHERE \"region_id\" = {r01} ORDER BY \"id\" LIMIT 1 OFFSET 5"))?;
            m.time(reps, |m, cold| record(&mut local, m, cold, "Customer", c))
        }
        "record_region" => {
            let r: u64 = r01.parse().map_err(|_| "bad id")?;
            m.time(reps, |m, cold| record(&mut local, m, cold, "Region", r))?;
            // its customers come bounded: a page and their count, not every one of them
            let customers: u64 = one(&mut conn, &format!("SELECT CAST(COUNT(*) AS TEXT) FROM \"customer\" WHERE \"region_id\" = {r}"), &[])?.parse().unwrap_or(0);
            let (_, body) = local.record(admin, "Region", r);
            let c = &body["cells"]["customers"];
            let shown = c["v"]["list"].as_array().map_or(0, |l| l.len()) as u64;
            if c["count"].as_u64() != Some(customers) || shown > quadra_api::LINES_PAGE as u64 || (customers > shown && c["next"].as_str().is_none()) {
                return Err(format!("the customers of Region#{r} are not bounded: {shown} shown, count {}, {customers} in the region", c["count"]));
            }
            let loaded = m.extra.get("loaded_records").and_then(|v| v.as_u64()).unwrap_or(0);
            if loaded > 4 * quadra_api::LINES_PAGE as u64 {
                return Err(format!("opening Region#{r} loaded {loaded} records for {customers} customers"));
            }
            m.set("note", json!(format!("Region#{r}, {loaded} records loaded, {shown} of its {customers} customers shown")));
            Ok(())
        }
        "record_region_more" => {
            // the next page of a region's customers, from the record's cursor (the panel scrolls)
            let r: u64 = r01.parse().map_err(|_| "bad id")?;
            let (status, body) = local.record(admin, "Region", r);
            if status != 200 {
                return Err(format!("{status}: {body}"));
            }
            let Some(next) = body["cells"]["customers"]["next"].as_str().map(str::to_string) else {
                return Err("the region's customers fit in one page".into());
            };
            let q = json!({ "entity": "Customer", "fields": [], "refs": { "region": r }, "limit": quadra_api::LINES_PAGE, "after": next, "count": false });
            local.clear_cache();
            m.time(reps, |m, cold| query(&mut local, m, cold, admin, &q))
        }
        "record_frozen" => {
            let o = id(&mut conn, "SELECT CAST(\"id\" AS TEXT) FROM \"order\" WHERE \"state\" = 'Cancelled' AND \"_frozen_tx\" IS NOT NULL ORDER BY \"id\" LIMIT 1")?;
            m.time(reps, |m, cold| record(&mut local, m, cold, "Order", o))
        }
        "commit_qty" => {
            let line = id(&mut conn, &format!("SELECT CAST(l.\"id\" AS TEXT) FROM \"order_line\" l JOIN \"order\" o ON o.\"id\" = l.\"order_id\" JOIN \"customer\" c ON c.\"id\" = o.\"customer_id\" WHERE o.\"state\" = 'Confirmed' AND c.\"region_id\" = {r01} ORDER BY l.\"id\" LIMIT 1 OFFSET 3"))?;
            let qty = one(&mut conn, "SELECT CAST(\"qty\" AS TEXT) FROM \"order_line\" WHERE \"id\" = ?bigint", &[Sql::Int(line as i64)])?;
            let qty: f64 = qty.parse().map_err(|_| "bad qty")?;
            let mut n = 0u64;
            let mut base = head(&mut conn)?;
            m.time(reps.max(10), |m, cold| {
                n += 1;
                let q = if n % 2 == 1 { qty + 1.0 } else { qty };
                let body = json!({ "base": base, "ops": [{ "op": "update", "entity": "OrderLine", "id": line, "values": { "qty": format!("{q} pcs") } }] });
                let (status, r) = local.tx(admin, &body, &format!("bench-qty-{}-{n}", std::process::id()));
                if status != 200 {
                    return Err(format!("{status}: {r}"));
                }
                base = r["base"].as_u64().unwrap_or(base + 1);
                if cold {
                    let (_, misses, _, _) = local.cache();
                    m.set("loaded_records", json!(misses));
                    m.set("note", json!(format!("OrderLine#{line}, {misses} records loaded by the first commit")));
                }
                Ok(())
            })
        }
        "commit_shipment" => {
            let rows = conn.query(
                &format!("SELECT CAST(l.\"id\" AS TEXT), CAST(l.\"qty\" AS TEXT), CAST(l.\"shipped\" AS TEXT) FROM \"order_line\" l JOIN \"order\" o ON o.\"id\" = l.\"order_id\" JOIN \"customer\" c ON c.\"id\" = o.\"customer_id\" WHERE o.\"state\" = 'Confirmed' AND c.\"region_id\" = {r01} ORDER BY l.\"id\" LIMIT 5000"),
                &[],
            )?;
            let free: Vec<u64> = rows
                .iter()
                .filter(|r| {
                    let n = |i: usize| r[i].as_deref().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                    n(1) - n(2) >= 1.0
                })
                .filter_map(|r| r[0].as_deref().and_then(|v| v.parse().ok()))
                .collect();
            if free.is_empty() {
                return Err("no line left to ship".into());
            }
            let mut n = 0usize;
            let mut base = head(&mut conn)?;
            m.time(reps.max(10), |m, cold| {
                let line = free[n % free.len()];
                n += 1;
                let body = json!({ "base": base, "ops": [{ "op": "create", "entity": "Shipment", "values": { "line": line, "qty": "1 pcs", "date": "2027-01-04", "carrier": "Air" } }] });
                let (status, r) = local.tx(admin, &body, &format!("bench-ship-{}-{n}", std::process::id()));
                if status != 200 {
                    return Err(format!("{status}: {r}"));
                }
                base = r["base"].as_u64().unwrap_or(base + 1);
                if cold {
                    let (_, misses, _, _) = local.cache();
                    m.set("loaded_records", json!(misses));
                    m.set("note", json!(format!("Shipment of OrderLine#{line}, {misses} records loaded by the first commit")));
                }
                Ok(())
            })
        }
        "commit_category" => {
            // a product sold in confirmed orders moves between two categories, and back
            let p = id(&mut conn, "SELECT CAST(p.\"id\" AS TEXT) FROM \"product\" p WHERE p.\"active\" AND EXISTS (SELECT 1 FROM \"order_line\" l JOIN \"order\" o ON o.\"id\" = l.\"order_id\" WHERE l.\"product_id\" = p.\"id\" AND o.\"state\" = 'Confirmed') ORDER BY p.\"id\" LIMIT 1 OFFSET 7")?;
            let from = id(&mut conn, &format!("SELECT CAST(\"category_id\" AS TEXT) FROM \"product\" WHERE \"id\" = {p}"))?;
            let to = id(&mut conn, &format!("SELECT CAST(\"id\" AS TEXT) FROM \"category\" WHERE \"active\" AND \"id\" <> {from} ORDER BY \"id\" LIMIT 1 OFFSET 3"))?;
            let mut n = 0u64;
            let mut base = head(&mut conn)?;
            m.time(reps.max(10), |m, cold| {
                n += 1;
                let c = if n % 2 == 1 { to } else { from };
                let body = json!({ "base": base, "ops": [{ "op": "update", "entity": "Product", "id": p, "values": { "category": c } }] });
                let (status, r) = local.tx(admin, &body, &format!("bench-category-{}-{n}", std::process::id()));
                if status != 200 {
                    return Err(format!("{status}: {r}"));
                }
                base = r["base"].as_u64().unwrap_or(base + 1);
                if cold {
                    let (_, misses, _, _) = local.cache();
                    m.set("loaded_records", json!(misses));
                    m.set("note", json!(format!("Product#{p} between Category#{from} and #{to}, {misses} records loaded by the first commit")));
                }
                Ok(())
            })
        }
        "commit_tree" => {
            // a category with a small subtree moves under another root, and back
            let c = id(&mut conn, "SELECT CAST(c.\"id\" AS TEXT) FROM \"category\" c WHERE c.\"parent_id\" IS NOT NULL AND EXISTS (SELECT 1 FROM \"category\" k WHERE k.\"parent_id\" = c.\"id\") ORDER BY c.\"id\" DESC LIMIT 1")?;
            let from = id(&mut conn, &format!("SELECT CAST(\"parent_id\" AS TEXT) FROM \"category\" WHERE \"id\" = {c}"))?;
            let to = id(&mut conn, &format!("SELECT CAST(\"id\" AS TEXT) FROM \"category\" WHERE \"parent_id\" IS NULL AND \"id\" <> {from} ORDER BY \"id\" LIMIT 1"))?;
            let mut n = 0u64;
            let mut base = head(&mut conn)?;
            m.time(reps.max(10), |m, cold| {
                n += 1;
                let p = if n % 2 == 1 { to } else { from };
                let body = json!({ "base": base, "ops": [{ "op": "update", "entity": "Category", "id": c, "values": { "parent": p } }] });
                let (status, r) = local.tx(admin, &body, &format!("bench-tree-{}-{n}", std::process::id()));
                if status != 200 {
                    return Err(format!("{status}: {r}"));
                }
                base = r["base"].as_u64().unwrap_or(base + 1);
                if cold {
                    let (_, misses, _, _) = local.cache();
                    m.set("loaded_records", json!(misses));
                    m.set("note", json!(format!("Category#{c} between Category#{from} and #{to}, {misses} records loaded by the first commit")));
                }
                Ok(())
            })
        }
        "commit_move" | "move_read" => {
            // a customer of R01 (or R02, where a previous run left it) moves to the other region:
            // the region of its orders, lines and shipments changes, and with it who may read them
            let r02 = one(&mut conn, "SELECT CAST(\"id\" AS TEXT) FROM \"region\" WHERE \"code\" = 'R02'", &[])?;
            let (r1, r2): (u64, u64) = (r01.parse().map_err(|_| "bad id")?, r02.parse().map_err(|_| "bad id")?);
            let offset = if name == "commit_move" { 7 } else { 9 };
            let c = id(&mut conn, &format!("SELECT CAST(\"id\" AS TEXT) FROM \"customer\" WHERE \"region_id\" IN ({r1}, {r2}) AND \"active\" ORDER BY \"id\" LIMIT 1 OFFSET {offset}"))?;
            let managers = (user(&mut conn, "manager01")?, user(&mut conn, "manager02")?);
            let orders_in = |conn: &mut Conn, r: u64| -> Result<u64, String> {
                one(conn, &format!("SELECT CAST(COUNT(*) AS TEXT) FROM \"order\" o JOIN \"customer\" c ON c.\"id\" = o.\"customer_id\" WHERE c.\"region_id\" = {r}"), &[])?.parse().map_err(|_| "bad count".to_string())
            };
            let mut base = head(&mut conn)?;
            let mut n = 0u64;
            let mut move_once = |local: &mut quadra_server::Local, conn: &mut Conn| -> Result<(u64, u64), String> {
                n += 1;
                let now_in: u64 = one(conn, "SELECT CAST(\"region_id\" AS TEXT) FROM \"customer\" WHERE \"id\" = ?bigint", &[Sql::Int(c as i64)])?.parse().map_err(|_| "bad id")?;
                let to = if now_in == r1 { r2 } else { r1 };
                let body = json!({ "base": base, "ops": [{ "op": "update", "entity": "Customer", "id": c, "values": { "region": to } }] });
                let (status, r) = local.tx(admin, &body, &format!("bench-move-{}-{n}", std::process::id()));
                if status != 200 {
                    return Err(format!("{status}: {r}"));
                }
                base = r["base"].as_u64().unwrap_or(base + 1);
                Ok((to, if to == r1 { managers.0 } else { managers.1 }))
            };
            if name == "commit_move" {
                // a few: each recomputes the rollups of two regions (phase 3)
                m.time(reps.min(2), |m, cold| {
                    move_once(&mut local, &mut conn)?;
                    if cold {
                        let (_, misses, _, _) = local.cache();
                        m.set("loaded_records", json!(misses));
                        m.set("note", json!(format!("Customer#{c} moved, {misses} records loaded by the first commit")));
                    }
                    Ok(())
                })
            } else {
                // the commit is not timed (commit_move measures it): the read after it is
                for k in 0..=reps {
                    let moved = move_once(&mut local, &mut conn)?;
                    let want = orders_in(&mut conn, moved.0)?;
                    let q = json!({ "entity": "Order", "limit": 500 });
                    let (s0, r0) = counters();
                    let d0 = quadra_store::conn::db_micros();
                    let t = Instant::now();
                    let (status, body) = local.query(moved.1, &q);
                    let took = ms(t);
                    if status != 200 {
                        return Err(format!("{status}: {body}"));
                    }
                    if body["total"].as_u64() != Some(want) {
                        return Err(format!("the new manager reads {} orders, the region has {want}", body["total"]));
                    }
                    let (s1, r1) = counters();
                    if k == 0 {
                        m.cold = Some(took);
                        m.set("cold_statements", json!(s1 - s0));
                        m.set("cold_records_loaded", json!(r1 - r0));
                        m.set("cold_db_ms", json!(((quadra_store::conn::db_micros() - d0) as f64 / 100.0).round() / 10.0));
                        m.set("note", json!(format!("Customer#{c} moved, then the {want} orders of its new region read as its manager")));
                    } else {
                        m.warm.push(took);
                    }
                    m.emit(false);
                }
                Ok(())
            }
        }
        other => Err(format!("unknown scenario `{other}` ({})", STEPS.iter().map(|s| s.0).collect::<Vec<_>>().join(", "))),
    }
}

fn sql(url: &str, name: &str, reps: usize, m: &mut Meter) -> Result<(), String> {
    let mut conn = Conn::open(url)?;
    let r01 = one(
        &mut conn,
        "SELECT CAST(\"id\" AS TEXT) FROM \"region\" WHERE \"code\" = 'R01'",
        &[],
    )?;
    let code = one(&mut conn, &format!("SELECT \"code\" FROM \"customer\" WHERE \"region_id\" = {r01} ORDER BY \"id\" LIMIT 1 OFFSET 5"), &[])?;
    // a NUMERIC column is TEXT on SQLite: compare it as a number there
    let num = |c: &str| {
        if conn.dialect() == Dialect::Sqlite {
            format!("CAST({c} AS REAL)")
        } else {
            c.to_string()
        }
    };
    let q = match name {
        "sql_count" => "SELECT CAST(COUNT(*) AS TEXT) FROM \"shipment\"".to_string(),
        "sql_page_sorted" => "SELECT CAST(\"id\" AS TEXT), CAST(\"date\" AS TEXT) FROM \"shipment\" ORDER BY \"date\" DESC, \"id\" LIMIT 500".to_string(),
        "sql_where3" => format!(
            "SELECT CAST(s.\"id\" AS TEXT) FROM \"shipment\" s JOIN \"order_line\" l ON l.\"id\" = s.\"line_id\" JOIN \"order\" o ON o.\"id\" = l.\"order_id\" JOIN \"customer\" c ON c.\"id\" = o.\"customer_id\" WHERE c.\"code\" = '{code}' ORDER BY s.\"id\" LIMIT 500"
        ),
        "sql_region_revenue" => format!("SELECT r.\"code\", CAST(SUM({}) AS TEXT) FROM \"customer\" c JOIN \"region\" r ON r.\"id\" = c.\"region_id\" GROUP BY r.\"code\" ORDER BY r.\"code\"", num("c.\"revenue\"")),
        "sql_top_customers" => format!("SELECT \"code\", CAST(\"revenue\" AS TEXT) FROM \"customer\" ORDER BY {} DESC LIMIT 10", num("\"revenue\"")),
        "sql_prefix" => "SELECT CAST(\"id\" AS TEXT), \"name\" FROM \"customer\" WHERE \"name\" LIKE 'Rossi%' ORDER BY \"id\" LIMIT 50".to_string(),
        other => return Err(format!("unknown scenario `{other}`")),
    };
    m.time(reps, |m, cold| {
        let rows = conn.query(&q, &[])?;
        if cold {
            m.set("rows", json!(rows.len()));
            m.set("note", json!(format!("{} rows", rows.len())));
        }
        Ok(())
    })
}

/// `quadra db verify` on the loaded database: it must find nothing.
fn verify(path: &Path, url: &str, m: &mut Meter) -> Result<(), String> {
    let (_, checked) = checked(path)?;
    let t = Instant::now();
    let (problems, records) =
        quadra_store::ops::verify(url, checked.model.as_ref().unwrap(), &checked.report, now())?;
    m.cold = Some(ms(t));
    if !problems.is_empty() {
        return Err(format!(
            "verify found {} problems: {}",
            problems.len(),
            problems
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    m.set("records", json!(records));
    m.set("note", json!(format!("{records} records checked, clean")));
    Ok(())
}

/// The project as a scenario changes it: a copy with every change up to `name` (they build
/// on each other, as successive saves would).
fn changed_copy(root: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("quadra-bench-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // tests are left out: they use the names a rename changes
    for entry in std::fs::read_dir(root)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let p = entry.path();
        if p.is_file() && matches!(p.extension().and_then(|x| x.to_str()), Some("qd" | "toml")) {
            std::fs::copy(&p, dir.join(entry.file_name())).map_err(|e| e.to_string())?;
        }
    }
    let order = [
        "deploy_noop",
        "deploy_add_field",
        "deploy_add_default",
        "deploy_rename",
        "deploy_add_check",
        "deploy_add_required",
        "deploy_add_where",
        "deploy_add_unique",
    ];
    let upto = order
        .iter()
        .position(|n| *n == name)
        .ok_or_else(|| format!("unknown scenario `{name}`"))?;
    let sales = dir.join("sales.qd");
    let mut text = std::fs::read_to_string(&sales).map_err(|e| e.to_string())?;
    let insert = |text: &mut String, after: &str, add: &str| -> Result<(), String> {
        let at = text
            .find(after)
            .ok_or_else(|| format!("sales.qd: `{after}` not found"))?
            + after.len();
        let eol = text[at..]
            .find('\n')
            .map(|i| at + i + 1)
            .unwrap_or(text.len());
        text.insert_str(eol, add);
        Ok(())
    };
    if upto >= 1 {
        insert(&mut text, "    salesperson: User", "    segment: Text?\n")?;
    }
    if upto >= 2 {
        insert(
            &mut text,
            "    priority: enum(Low, Normal, High) default Normal",
            "    channel: enum(Web, Phone, Agent) default Web\n",
        )?;
    }
    if upto >= 3 {
        text = text.replacen(
            "    code: Code unique\n    name: Text\n    region: Region",
            "    code: Code unique\n    title: Text\n    region: Region",
            1,
        );
        std::fs::create_dir_all(dir.join("migrations")).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("migrations/bench_rename.qd"), "migration bench_rename:\n    reason: \"Customers have a title\"\n    rename Customer.name -> Customer.title\n").map_err(|e| e.to_string())?;
    }
    if upto >= 4 {
        insert(
            &mut text,
            "    discount: Percent where 0%..50% default 0%",
            "    check qty <= 1000 pcs, \"At most 1000 pieces per line\"\n",
        )?;
    }
    // the shipments' tracking code: required for the big ones, of a form, unique
    let tracking = match upto {
        5 => Some("    tracking: Code? required when qty > 1000 pcs\n"),
        6 => Some("    tracking: Code? where t => t.starts_with(\"TR\") required when qty > 1000 pcs\n"),
        7 => Some("    tracking: Code? where t => t.starts_with(\"TR\") required when qty > 1000 pcs\n    unique (tracking)\n"),
        _ => None,
    };
    if let Some(t) = tracking {
        if !text.contains("    tracking: Code?\n") {
            return Err("sales.qd: `tracking: Code?` not found".into());
        }
        text = text.replacen("    tracking: Code?\n", t, 1);
    }
    std::fs::write(&sales, text).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn deploy(path: &Path, url: &str, name: &str, m: &mut Meter) -> Result<(), String> {
    let root = quadra_sema::find_project_root(path).ok_or("no quadra.toml")?;
    let dir = changed_copy(&root, name)?;
    let r = (|| {
        let project = quadra_sema::Project::load(&dir).map_err(|e| e.to_string())?;
        let checked = quadra_analysis::check(&project);
        let Some(model) = &checked.model else {
            let errors: Vec<String> = checked
                .diagnostics
                .iter()
                .filter(|d| d.severity == quadra_sema::Severity::Error)
                .map(|d| d.message.clone())
                .collect();
            return Err(format!(
                "the changed project does not check: {}",
                errors.join("; ")
            ));
        };
        let t = Instant::now();
        let r = quadra_store::ops::deploy(
            url,
            model,
            &checked.report,
            &dir,
            now(),
            false,
            &format!("bench {name}"),
        )?;
        m.cold = Some(ms(t));
        let how = if r.incremental {
            format!(
                "{}",
                if r.steps.is_empty() {
                    "no table change".to_string()
                } else {
                    r.steps.join("; ")
                }
            )
        } else {
            format!(
                "tables rebuilt ({})",
                r.rebuilt_because.clone().unwrap_or_default()
            )
        };
        m.set(
            "note",
            json!(format!(
                "{how}; {} new default values, {} migrations",
                r.new_defaults,
                r.scripts.len()
            )),
        );
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    r
}
