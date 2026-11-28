# Tutorial: build a small shop

In this tutorial you build the back office of a small shop that sells coffee beans, tea and
mugs. You start with a single entity and finish with an application you could actually run:
customers, a catalog priced in euros per kilogram or per piece, orders with a life cycle,
invoices and payments, stock, permissions for each kind of employee, a database with an API,
the tools to change all of it safely once it holds real data, and the connections to
the systems around it (a web shop, a carrier, a till, a bank and the accountant's database),
and the documents it sends: invoices, labels, signed files.

Each step adds one idea to the same project. Every step is a complete, working project:
its code compiles with zero errors and all of its tests pass.

## What you'll build

| Step | You learn | The shop gets |
|---|---|---|
| [1. Your first entity](./01-first-entity) | entities, fields, `quadra check`, a first test | customers and products |
| [2. Money, quantities and units](./02-types-money-units) | `Money`, `Price`, `Qty`, `Percent`, dates; what the compiler refuses | prices per kg and per piece |
| [3. Relations](./03-relations) | references, optionals `?.` `??`, lists, `via`, `parent`, `through`, modules | categories, suppliers, orders with lines |
| [4. Formulas](./04-formulas) | formulas, lambdas, `sum`/`map`/`group_by`/`flat_map`, rollups, `when:` | totals, VAT, shipping, customer tiers |
| [5. Rules](./05-rules) | `check`, `warn`, `where`, `required when`, `unique` | data that is always valid |
| [6. Flows and actions](./06-flows-actions) | `flow`, `action`, `require`, `frozen when … except` | orders that are confirmed, shipped, cancelled |
| [7. Invoices: frozen and live](./07-invoices-frozen-live) | documents that follow documents, `live`, multi-hop rollups | invoices and payments |
| [8. Tests](./08-tests) | fixtures, `clock`, `expect_error`, `expect_compile_error`, `patch` | a real test suite |
| [9. Permissions](./09-permissions) | roles, row and field rules, `hide`, `deny`, `as user:` | clerks, warehouse, accountants |
| [10. Ledgers](./10-ledgers) | `ledger`, `post to`, automatic reversals | stock and receivables |
| [11. Database and API](./11-database-and-api) | `quadra db`, `quadra serve`, tokens, the HTTP API, the web client | a running app |
| [12. Migrations and changes](./12-migrations-and-changes) | `migration` (rename, convert, backfill, verify), provisional values, `change` | a model that evolves with real data |
| [13. Talking to other systems](./13-talking-to-other-systems) | `[vars]`, connections, `inbound`, `outbound`, `publish`, records `from json`, `receive` / `expect sent` / `deliver` | orders from a web shop, parcels, emails |
| [14. Files, formats and databases](./14-files-formats-and-databases) | SFTP and FTP, `from csv` and `from fixed`, `atomic`, `write`, `insert`, on-premise agents | the till, the bank, the accountant |
| [15. Documents, labels and signatures](./15-documents-labels-and-signatures) | `.qdp` files, `document`, `label`, `render`, `zpl`, `attach`, signers | invoices in PDF, parcel labels, signed files |
| [16. Retention, erasure and a signed history](./16-retention-erasure-and-signed-history) | `retain`, `expire`, `personal`, `erase`, signed commits, archives | GDPR requests, invoices kept ten years |
| [17. Pages, history and the trash](./17-pages-history-and-the-trash) | `history last 10`, `history none`, `deletable`, the trash, `[data]` | a site edited often, baskets that go |
| [Next steps](./18-next-steps) | where to go from here | |

## How each step works

Every step has the same shape:

1. **The goal** — what the shop needs next.
2. **The concept**, explained with the smallest piece of code that shows it.
3. **The code** of the step. Snippets are taken from the step's project; highlighted lines are
   what changed.
4. **Run it**: the commands, and the output you should see.
5. **Try it**: a small exercise to do on your own.
6. **Recap**.

At the top of each step there is a button:

<SandboxLink project="tutorial-01" file="shop.qd" />

It opens the complete project **as of the end of that step** in the [Sandbox](/sandbox), which
runs the compiler and the tests in your browser. You can follow the whole tutorial there, with
nothing installed.

If you prefer your own machine ([Installation](/guide/installation)), create a folder and
keep it for the whole tutorial: at each step, add or update the files shown, then run
`quadra check` and `quadra test` in that folder. From step 11 on you will also need a database
in that same folder, because step 12 migrates the data you created in step 11.

::: tip Conventions
- Commands run in the project folder (the one with `quadra.toml`).
- Output shown in `text` blocks is the real output of the commands on the step's project.
- `# comments` in the code explain what a line does; you don't have to type them.
:::

Ready? [Start with your first entity →](./01-first-entity)
