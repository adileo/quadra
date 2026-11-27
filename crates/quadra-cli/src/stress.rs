//! Deterministic data for `examples/stress` (`quadra bench gen`, `quadra bench run`): the
//! same seed and scale give the same rows, byte for byte, on every machine.
//!
//! The data satisfies every rule of the model: credit limits cover the open orders, shipped
//! orders are fully shipped, drafts use only products and categories still sold, inactive
//! customers have only cancelled orders, one region is locked (its books are closed).
//! Customers come region by region, orders customer by customer, lines order by order:
//! the locality real data has.

use std::fmt::Write as _;

/// How much data: (name, regions, categories, products, tags, customers).
/// Orders: ~7.5 per customer; lines: ~3.5 per order; shipments: ~0.8 per line.
pub const SCALES: &[(&str, usize, usize, usize, usize, usize)] = &[
    ("tiny", 3, 8, 40, 6, 40),
    ("small", 10, 60, 1_000, 20, 2_000),
    ("medium", 20, 200, 5_000, 50, 20_000),
    ("large", 40, 400, 20_000, 100, 100_000),
];

pub fn scale_names() -> String {
    SCALES.iter().map(|s| s.0).collect::<Vec<_>>().join(", ")
}

/// Where the rows go: entity by entity, a header then the rows.
pub trait Sink {
    fn entity(&mut self, name: &str, header: &[&str]) -> Result<(), String>;
    fn row(&mut self, cells: &[&str]) -> Result<(), String>;
}

/// splitmix64: tiny, fast, and the same everywhere.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// 0..n
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    /// a..=b
    pub fn range(&mut self, a: usize, b: usize) -> usize {
        a + self.below(b - a + 1)
    }

    /// true with probability p percent
    pub fn pct(&mut self, p: usize) -> bool {
        self.below(100) < p
    }
}

const FIRST: &[&str] = &[
    "Rossi", "Bianchi", "Verdi", "Ferrari", "Esposito", "Romano", "Colombo", "Ricci", "Marino",
    "Greco", "Bruno", "Gallo", "Conti", "De Luca", "Mancini", "Costa", "Giordano", "Rizzo",
    "Lombardi", "Moretti", "Acme", "Globex", "Initech", "Umbrella", "Stark", "Wayne", "Hooli",
    "Vandelay", "Soylent", "Tyrell",
];
const KIND: &[&str] = &[
    "Spa",
    "Srl",
    "& Figli",
    "Group",
    "Trading",
    "Logistics",
    "Industries",
    "Retail",
    "Foods",
    "Tech",
];
const WORDS: &[&str] = &[
    "Hammer", "Saw", "Drill", "Wrench", "Screw", "Bolt", "Nut", "Washer", "Pliers", "Chisel",
    "File", "Clamp", "Level", "Tape", "Glue", "Brush", "Roller", "Ladder", "Bucket", "Gloves",
];
const SIZES: &[&str] = &[
    "Mini", "Small", "Medium", "Large", "XL", "Pro", "Basic", "Plus",
];
pub const STATES: &[&str] = &["Draft", "Confirmed", "Shipped", "Cancelled"];
const PRIORITIES: &[&str] = &["Low", "Normal", "High"];
const CARRIERS: &[&str] = &["Road", "Rail", "Air"];
const COLORS: &[&str] = &["Grey", "Red", "Green", "Blue"];

fn money(cents: u64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

fn date(day: u32) -> String {
    // day 0 = 2024-01-01
    let d =
        chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap() + chrono::Duration::days(day as i64);
    d.to_string()
}

struct OrderInfo {
    customer: u32,
    day: u32,
    state: u8,
    priority: u8,
    first_line: u32,
    lines: u8,
}

struct LineInfo {
    product: u32,
    qty: u8,
    /// a negotiated price in cents (none: the product's list price, by default)
    price: Option<u32>,
    discount: u8,
}

struct ShipInfo {
    line: u32,
    qty: u8,
    day: u32,
    carrier: u8,
    tracking: bool,
}

/// What the generator produced: counts per entity, and the logins of the users by role.
#[derive(Debug, Default)]
pub struct Generated {
    pub counts: Vec<(String, usize)>,
}

impl Generated {
    pub fn total(&self) -> usize {
        self.counts.iter().map(|c| c.1).sum()
    }
}

/// Logins the benchmark uses: they exist at every scale.
pub const ADMIN: &str = "admin";
pub const SALESPERSON: &str = "sales0001";
pub const AUDITOR: &str = "auditor";
pub const CONTROLLER: &str = "controller";

/// Generate every entity of the stress model into `out`.
pub fn generate(scale: &str, seed: u64, out: &mut dyn Sink) -> Result<Generated, String> {
    let &(_, regions, ncat, nprod, ntags, ncust) = SCALES
        .iter()
        .find(|s| s.0 == scale)
        .ok_or_else(|| format!("unknown scale `{scale}` ({})", scale_names()))?;
    let mut rng = Rng::new(seed);
    let mut g = Generated::default();
    let count = |name: &str, n: usize, g: &mut Generated| g.counts.push((name.to_string(), n));

    // ---- users: admin, auditor, controller, a manager per region, logistics, salespeople ----
    let nsales = (ncust / 100).max(4);
    let nlog = regions.div_ceil(4);
    out.entity("User", &["id", "login", "name", "roles"])?;
    out.row(&["admin", ADMIN, "Admin", "Admin"])?;
    out.row(&["auditor", AUDITOR, "Auditor", "Auditor"])?;
    out.row(&["controller", CONTROLLER, "Controller", "Controller"])?;
    for r in 0..regions {
        let login = format!("manager{:02}", r + 1);
        out.row(&[
            &format!("m{r}"),
            &login,
            &format!("Manager {}", r + 1),
            "RegionalManager",
        ])?;
    }
    for l in 0..nlog {
        let login = format!("logistics{:02}", l + 1);
        out.row(&[
            &format!("l{l}"),
            &login,
            &format!("Logistics {}", l + 1),
            "Logistics",
        ])?;
    }
    for s in 0..nsales {
        let login = format!("sales{:04}", s + 1);
        out.row(&[
            &format!("s{s}"),
            &login,
            &format!("Salesperson {}", s + 1),
            "Salesperson",
        ])?;
    }
    count("User", 3 + regions + nlog + nsales, &mut g);
    // salespeople by region (round robin): the customers of a salesperson are in one region
    let sales_of = |r: usize| -> Vec<usize> { (0..nsales).filter(|s| s % regions == r).collect() };

    // ---- regions: the last one is locked ----
    out.entity(
        "Region",
        &[
            "id",
            "code",
            "name",
            "manager",
            "logistics",
            "team",
            "active",
            "locked",
            "target",
        ],
    )?;
    for r in 0..regions {
        let team: Vec<String> = (0..3).map(|_| format!("s{}", rng.below(nsales))).collect();
        let locked = if r + 1 == regions && regions > 1 {
            "true"
        } else {
            "false"
        };
        let target = money(rng.range(1_000, 50_000) as u64 * 100_000);
        out.row(&[
            &r.to_string(),
            &format!("R{:02}", r + 1),
            &format!("Region {}", r + 1),
            &format!("m{r}"),
            &format!("l{}", r / 4),
            &team.join(";"),
            "true",
            locked,
            &format!("{target} EUR"),
        ])?;
    }
    count("Region", regions, &mut g);

    // ---- categories: a tree; a few leaves are not sold any more ----
    let mut parent: Vec<Option<usize>> = Vec::with_capacity(ncat);
    for c in 0..ncat {
        parent.push(if c < (ncat / 10).max(1) {
            None
        } else {
            Some(rng.below(c))
        });
    }
    let mut has_children = vec![false; ncat];
    for p in parent.iter().flatten() {
        has_children[*p] = true;
    }
    let cat_active: Vec<bool> = (0..ncat).map(|c| has_children[c] || !rng.pct(3)).collect();
    out.entity("Category", &["id", "code", "name", "parent", "active"])?;
    for c in 0..ncat {
        let name = format!("{} {}", WORDS[c % WORDS.len()], c + 1);
        let p = parent[c].map(|p| p.to_string()).unwrap_or_default();
        out.row(&[
            &c.to_string(),
            &format!("C{:04}", c + 1),
            &name,
            &p,
            if cat_active[c] { "true" } else { "false" },
        ])?;
    }
    count("Category", ncat, &mut g);

    // ---- products ----
    let mut price = Vec::with_capacity(nprod);
    let mut sellable = Vec::with_capacity(nprod);
    out.entity(
        "Product",
        &["id", "sku", "name", "category", "price", "cost", "active"],
    )?;
    for p in 0..nprod {
        let cat = rng.below(ncat);
        let cents = rng.range(100, 20_000) as u32;
        let active = !rng.pct(2);
        price.push(cents);
        sellable.push(active && cat_active[cat]);
        let name = format!(
            "{} {} {}",
            WORDS[rng.below(WORDS.len())],
            SIZES[rng.below(SIZES.len())],
            p + 1
        );
        out.row(&[
            &p.to_string(),
            &format!("P{:06}", p + 1),
            &name,
            &cat.to_string(),
            &format!("{} EUR/pcs", money(cents as u64)),
            &format!("{} EUR", money(cents as u64 * 6 / 10)),
            if active { "true" } else { "false" },
        ])?;
    }
    count("Product", nprod, &mut g);
    let sold: Vec<u32> = (0..nprod as u32)
        .filter(|p| sellable[*p as usize])
        .collect();

    // ---- tags ----
    out.entity("Tag", &["id", "name", "color"])?;
    for t in 0..ntags {
        out.row(&[
            &t.to_string(),
            &format!("TAG{:03}", t + 1),
            COLORS[rng.below(COLORS.len())],
        ])?;
    }
    count("Tag", ntags, &mut g);

    // ---- the documents, decided first: credit limits must cover the open orders ----
    // customers region by region; 2% are inactive, with cancelled orders only
    let mut cust_region = Vec::with_capacity(ncust);
    for c in 0..ncust {
        cust_region.push(c * regions / ncust);
    }
    let cust_active: Vec<bool> = (0..ncust).map(|_| !rng.pct(2)).collect();
    let mut orders: Vec<OrderInfo> = Vec::with_capacity(ncust * 8);
    let mut lines: Vec<LineInfo> = Vec::with_capacity(ncust * 27);
    let mut ships: Vec<ShipInfo> = Vec::with_capacity(ncust * 22);
    let mut open_bound = vec![0u64; ncust];
    for c in 0..ncust {
        let n = rng.range(1, 14);
        for _ in 0..n {
            let day = rng.range(0, 1000) as u32;
            let state = if !cust_active[c] {
                3
            } else {
                match rng.below(100) {
                    0..=14 => 0,
                    15..=49 => 1,
                    50..=94 => 2,
                    _ => 3,
                }
            };
            let nl = rng.range(1, 6) as u8;
            let first_line = lines.len() as u32;
            for _ in 0..nl {
                let product = if state == 0 {
                    sold[rng.below(sold.len())]
                } else {
                    rng.below(nprod) as u32
                };
                let qty = rng.range(1, 20) as u8;
                let negotiated = rng.pct(30).then(|| price[product as usize] * 95 / 100);
                let discount = if rng.pct(80) {
                    0
                } else {
                    [5, 10, 15, 20][rng.below(4)]
                };
                let unit = negotiated.unwrap_or(price[product as usize]) as u64;
                if state == 1 {
                    open_bound[c] += unit * qty as u64;
                }
                let line = lines.len() as u32;
                lines.push(LineInfo {
                    product,
                    qty,
                    price: negotiated,
                    discount,
                });
                // shipped orders: every line shipped, in one or two shipments; confirmed
                // orders: some lines partly shipped
                let to_ship = match state {
                    2 => qty,
                    1 if rng.pct(40) => rng.range(1, qty as usize) as u8,
                    _ => 0,
                };
                if to_ship > 0 {
                    let split = if to_ship > 1 && rng.pct(40) {
                        rng.range(1, to_ship as usize - 1) as u8
                    } else {
                        to_ship
                    };
                    for part in [split, to_ship - split] {
                        if part > 0 {
                            ships.push(ShipInfo {
                                line,
                                qty: part,
                                day: day + rng.range(1, 20) as u32,
                                carrier: rng.below(3) as u8,
                                tracking: rng.pct(70),
                            });
                        }
                    }
                }
            }
            orders.push(OrderInfo {
                customer: c as u32,
                day,
                state,
                priority: rng.below(3) as u8,
                first_line,
                lines: nl,
            });
        }
    }

    // ---- customers ----
    out.entity(
        "Customer",
        &[
            "id",
            "code",
            "name",
            "region",
            "salesperson",
            "tags",
            "credit_limit",
            "active",
        ],
    )?;
    for c in 0..ncust {
        let r = cust_region[c];
        let reps = sales_of(r);
        let rep = reps.get(rng.below(reps.len())).copied().unwrap_or(0);
        let tags: Vec<String> = (0..rng.range(0, 3))
            .map(|_| rng.below(ntags).to_string())
            .collect();
        let name = format!(
            "{} {} {}",
            FIRST[rng.below(FIRST.len())],
            KIND[rng.below(KIND.len())],
            c + 1
        );
        let limit = (open_bound[c] + 100_000).max(5_000_000) / 100_000 * 100_000 + 100_000;
        out.row(&[
            &c.to_string(),
            &format!("K{:07}", c + 1),
            &name,
            &r.to_string(),
            &format!("s{rep}"),
            &tags.join(";"),
            &format!("{} EUR", money(limit)),
            if cust_active[c] { "true" } else { "false" },
        ])?;
    }
    count("Customer", ncust, &mut g);

    out.entity(
        "Order",
        &["id", "number", "customer", "date", "state", "priority"],
    )?;
    for (i, o) in orders.iter().enumerate() {
        out.row(&[
            &i.to_string(),
            &format!("O{:08}", i + 1),
            &o.customer.to_string(),
            &date(o.day),
            STATES[o.state as usize],
            PRIORITIES[o.priority as usize],
        ])?;
    }
    count("Order", orders.len(), &mut g);

    out.entity(
        "OrderLine",
        &["id", "order", "product", "qty", "price", "discount"],
    )?;
    let mut buf = String::new();
    for (i, o) in orders.iter().enumerate() {
        for k in 0..o.lines as u32 {
            let l = &lines[(o.first_line + k) as usize];
            buf.clear();
            if let Some(p) = l.price {
                let _ = write!(buf, "{} EUR/pcs", money(p as u64));
            }
            let discount = if l.discount == 0 {
                String::new()
            } else {
                format!("{}%", l.discount)
            };
            out.row(&[
                &(o.first_line + k).to_string(),
                &i.to_string(),
                &l.product.to_string(),
                &format!("{} pcs", l.qty),
                &buf,
                &discount,
            ])?;
        }
    }
    count("OrderLine", lines.len(), &mut g);

    out.entity(
        "Shipment",
        &["id", "line", "qty", "date", "carrier", "tracking"],
    )?;
    for (i, s) in ships.iter().enumerate() {
        let tracking = if s.tracking {
            format!("TR{:09}", i + 1)
        } else {
            String::new()
        };
        out.row(&[
            &i.to_string(),
            &s.line.to_string(),
            &format!("{} pcs", s.qty),
            &date(s.day),
            CARRIERS[s.carrier as usize],
            &tracking,
        ])?;
    }
    count("Shipment", ships.len(), &mut g);
    Ok(g)
}

/// CSV files, one per entity, in a directory (`quadra db import` reads them back).
pub struct CsvDir {
    dir: std::path::PathBuf,
    file: Option<std::io::BufWriter<std::fs::File>>,
    line: String,
}

impl CsvDir {
    pub fn new(dir: &std::path::Path) -> Result<CsvDir, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        Ok(CsvDir {
            dir: dir.to_path_buf(),
            file: None,
            line: String::new(),
        })
    }

    fn write(&mut self, cells: &[&str]) -> Result<(), String> {
        use std::io::Write;
        self.line.clear();
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                self.line.push(',');
            }
            self.line.push_str(&quadra_store::bulk::csv_cell(c));
        }
        self.line.push('\n');
        self.file
            .as_mut()
            .ok_or("no entity begun")?
            .write_all(self.line.as_bytes())
            .map_err(|e| e.to_string())
    }

    pub fn finish(&mut self) -> Result<(), String> {
        use std::io::Write;
        if let Some(mut f) = self.file.take() {
            f.flush().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

impl Sink for CsvDir {
    fn entity(&mut self, name: &str, header: &[&str]) -> Result<(), String> {
        self.finish()?;
        let path = self.dir.join(format!("{name}.csv"));
        let f = std::fs::File::create(&path)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        self.file = Some(std::io::BufWriter::with_capacity(1 << 20, f));
        self.write(header)
    }

    fn row(&mut self, cells: &[&str]) -> Result<(), String> {
        self.write(cells)
    }
}

/// Straight into a bulk import, without files.
pub struct ToImport<'a, 'm> {
    pub import: &'a mut quadra_store::bulk::Import<'m>,
    handle: Option<usize>,
    entity: String,
    line: usize,
}

impl<'a, 'm> ToImport<'a, 'm> {
    pub fn new(import: &'a mut quadra_store::bulk::Import<'m>) -> Self {
        ToImport {
            import,
            handle: None,
            entity: String::new(),
            line: 0,
        }
    }
}

impl Sink for ToImport<'_, '_> {
    fn entity(&mut self, name: &str, header: &[&str]) -> Result<(), String> {
        let header: Vec<String> = header.iter().map(|h| h.to_string()).collect();
        self.entity = name.to_string();
        self.handle = Some(
            self.import
                .entity(name, &header, &format!("generated {name}"))?,
        );
        self.line = 1;
        Ok(())
    }

    fn row(&mut self, cells: &[&str]) -> Result<(), String> {
        self.line += 1;
        self.import
            .row(self.handle.ok_or("no entity begun")?, cells, self.line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Count(Vec<(String, usize)>, u64);

    impl Sink for Count {
        fn entity(&mut self, name: &str, _header: &[&str]) -> Result<(), String> {
            self.0.push((name.to_string(), 0));
            Ok(())
        }
        fn row(&mut self, cells: &[&str]) -> Result<(), String> {
            self.0.last_mut().unwrap().1 += 1;
            for c in cells {
                for b in c.bytes() {
                    self.1 = self.1.wrapping_mul(31).wrapping_add(b as u64);
                }
            }
            Ok(())
        }
    }

    /// The same seed gives the same rows; another seed other rows.
    #[test]
    fn generation_is_deterministic() {
        let run = |seed| {
            let mut c = Count(Vec::new(), 0);
            let g = generate("tiny", seed, &mut c).unwrap();
            assert_eq!(g.counts, c.0);
            (c.0, c.1)
        };
        let a = run(7);
        assert_eq!(a, run(7));
        assert_ne!(a.1, run(8).1);
        let names: Vec<&str> = a.0.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "User",
                "Region",
                "Category",
                "Product",
                "Tag",
                "Customer",
                "Order",
                "OrderLine",
                "Shipment"
            ]
        );
    }
}
