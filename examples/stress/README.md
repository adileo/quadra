# Stress test

A model made to measure Quadra on millions of rows, with every feature that makes large data
hard on purpose:

```text
Region ← Customer ← Order ← OrderLine ← Shipment        (parent lines and references)
Category ← Category (a tree) ← Product                   Customer.tags: {Tag} (many-to-many)
```

| Feature | Where |
|---|---|
| Rollups at every level, rollups of rollups | `OrderLine.shipped`, `Order.total`/`to_ship`, `Customer.revenue`/`open_total`/`to_ship`, `Region.revenue`/`to_ship`; `Category.units_sold` from `Product.units_sold` |
| Counts, min, max, an average | `line_count`, `order_count`, `first_order`/`last_order`, `biggest_line`, `best_customer`, `avg_order` |
| Checks 1, 2, 3 and 4 relations away | `Customer`: `region.active`; `OrderLine`: `product…`, `product.category…`, `order.customer.region…`; `Shipment`: `line.order.date`, `line.order.customer.active`, `line.order.customer.region.last_delivery` |
| Checks on aggregates | `Customer.open_total <= credit_limit`, `OrderLine.shipped <= qty`, `Order.to_ship == 0` when shipped |
| Recursion over a tree | `Category.depth`, `Category.path` (proved acyclic by a `closure` check) |
| Many-to-many and its inverse | `Customer.tags` / `Tag.customers`, `Region.team: {User}` |
| Frozen documents | a cancelled `Order` (and its lines) is frozen |
| Row permissions through relations | `Salesperson` reads orders where `customer.salesperson == me or customer.region.manager == me`, customers through the region's team; `Logistics` reads shipments where `line.order.customer.region.logistics == me` (4 relations); lines and shipments follow their order |
| Queries over whole entities | `Region.review` in `campaigns.qd` (an action with `Customer.where`, `Order.count`, `.sum`, `.sort_by(…).first()`, a loop over `Order.where(…)`), `Campaign`'s check with `Order.count(…)` |
| `deny` through relations, hidden fields | the orders, customers and shipments of a `locked` region (edits, and a `Controller` with `read *` does not read its orders); `Customer.credit_limit`, `Product.cost` hidden from salespeople |

```bash
quadra test examples/stress                      # the features, on a small world
quadra bench gen --scale small --out /tmp/stress # ~100 000 rows as CSV files
quadra db init examples/stress --url sqlite:/tmp/stress.db
quadra db import /tmp/stress examples/stress --url sqlite:/tmp/stress.db
quadra bench run examples/stress --url sqlite:/tmp/stress.db --scale small --json small.json
```

`apps/sales-cockpit.json` is an app (a dashboard) to import with `quadra db views import
examples/stress/apps/sales-cockpit.json examples/stress --url …` (owner `admin`, shared with
everyone): filters on region, period and state, KPIs, the monthly revenue, the top customers,
the board of orders with buttons confirming or cancelling the selected one, a form creating an
order, a region × state pivot. The benchmark's `app_*` scenarios open it
(`site/reference/apps-and-dashboards.md`).

Scales: `tiny` (~2 000 rows), `small` (~100 000), `medium` (~1 000 000), `large` (~5 000 000).
The data is deterministic (`--seed`) and satisfies every rule: one region is locked, a few
customers are inactive with cancelled orders only, shipped orders are fully shipped.

`docs/SCALING.md` has the goals, the baseline numbers and the map of what limits them.
