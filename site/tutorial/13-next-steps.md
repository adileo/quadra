# Next steps

<SandboxLink project="tutorial-12" file="orders.qd" label="Open the finished shop" />

You've built a small but complete business application: a catalog with prices per unit,
customers with credit limits, orders with a life cycle, invoices and payments that follow
them, stock and receivables in ledgers, permissions for each role, a database with a verified
history, an API, and the tools to evolve all of it safely.

Here is where to go from here.

## Read a bigger model

The repository's `examples/extended` folder is a full ERP written in Quadra — partners,
products and price lists, multi-currency sales and purchasing, deliveries and receipts with
lots, manufacturing with multi-level bills of materials, double-entry accounting with a
general ledger, exchange differences, reorder planning, certified metrics and audit controls.
You can open it in the [Sandbox](/sandbox?project=erp). Things to look for, now that you know
the basics:

- **Multi-currency**: `Money<order.currency>` and `Money<company.currency>`, `convert(…)`,
  `exchange_rate(…)`, and how the compiler proves that two amounts are in the same currency
  (filters like `where(g => g.company == company)` count as proofs).
- **`per company` fields**: a value per company, in that company's currency
  (`list_price: Price<company.currency, uom> per company`).
- **Traits**: `trait Document` shared by orders, invoices and payments, and references to
  "any document" (`origin: Document?`).
- **Enums with data**: `enum PriceTarget: AllProducts, OneProduct(product: Product), …` and
  `match` with bindings.
- **Reactions**: `on SalesOrder enters Confirmed as o:` creates the delivery;
  `on ReorderRule becomes to_order > 0 as r converges:` reorders automatically;
  `before SalesOrder.confirm as o:` adds a credit check from another module.
- **`allocate`**: splitting an amount over items so that the shares add up to the cent
  (VAT per line, landed costs, payment terms).
- **Recursion on data**: category paths and bills of materials with `closure(…)`, made safe by
  a `check` that the tree has no cycles.
- **Metrics and controls**: `metric revenue(company, period) -> Money<…>` with fingerprints
  recorded by `quadra certify` in `metrics.lock`, so a changed definition doesn't go unnoticed.
- **Views and reports**: `view SalesOrderForm(o: SalesOrder)` and `report SalesAnalysis`.

## Go deeper in the reference

- [Reference](/reference/) — every construct of the language, the standard library, the CLI,
  the HTTP API and how storage works.
- `quadra trace Entity.field` on any value you don't understand.
- The editor extension's hover shows the **unified view** of an entity, with every `extend`
  from every module.

## Things to try on your shop

1. **Multi-company**: extend `Company` with an address and a VAT number, add
   `company: Company default current.company` to orders and invoices, and number documents per
   company.
2. **A reaction**: when an order enters `Confirmed`, create a `Shipment` document with one
   line per order line (`on Order enters Confirmed as o:`).
3. **Purchasing**: a `PurchaseOrder` to a `Supplier`, whose receipt creates `Restock`s — and
   a `ReorderRule` that proposes a purchase when `available` falls below a minimum.
4. **A general ledger**: a `GL` ledger with `check per (source): g => g.sum(r => r.amount) == 0`
   (double entry), posted by invoices and payments.
5. **PostgreSQL**: `quadra test --db postgres://…` and `quadra db init --url postgres://…` —
   the same model, the same history, the same hashes.

Happy modelling — and may your books always balance.
