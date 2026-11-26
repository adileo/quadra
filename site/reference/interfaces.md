# Interface files (`.qdi`)

An [app](/reference/apps-and-dashboards) or [saved views](/reference/views-and-boards) can be
written as a file of the project instead of being made in the web client: **`interfaces/**/*.qdi`**.
A file is the same app as text, with the same lexer, comments, literals and expressions as the
code: reviewed and diffed with the code, checked with it, deployed with it.

- **Checked with the model.** Every field, action, entity and expression of a file is checked by
  `quadra check` with the code: rename or remove a field in a `.qd` file and every `.qdi` that
  uses it is an error at its line — in `quadra check`, in `quadra dev`, before a deploy, and in
  the editor at once.
- **The path is the app.** `interfaces/sales/cockpit.qdi` is the app `sales/cockpit`, in the
  folder `sales`: the folders of the sidebar are the directories.
- **Read-only in production**, **written back in `quadra dev`**: changing a file's app in the
  web client while developing rewrites its file (in the canonical form below).

## An example

`examples/stress/interfaces/sales/cockpit.qdi` (the app of `examples/stress/apps/sales-cockpit.json`):

```python
app "Sales cockpit":
    description: "Filters (region, period, state), KPIs, monthly revenue, top customers, …"
    share: everyone use

    filter region "Region": multi Order.customer.region
    filter period "Period": date_range Order.date
    filter state "State": multi Order.state

    page cockpit "Cockpit":
        row height 2:
            filters filters width 24:
                show: region, period, state
                layout: row

        row height 3:
            kpi k_revenue "Revenue" width 8:
                source: Order.where(o => o.state in {Confirmed, Shipped}).sum(o => o.total)
                format: money
                decimals: 0
                compare: previous @period
            kpi k_orders "Orders" width 16:
                source: Order.count()

        row height 9:
            view v_board "Orders by state" width 16:
                of: Order
                type: kanban
                select: order
                edit: move
                by: state
                cards: customer, total, date
                sum: total
            column width 8:
                button b_confirm height 2:
                    label: "Confirm the selected order"
                    run: @order.confirm()
                    confirm: "Confirm this order? Its customer is told."
                form f_order "New order" height 7:
                    entity: Order
                    fields: number, customer, date, priority
                    submit_label: "Create the order"
```

and saved views, in `interfaces/sales/orders.qdi`:

```python
view "To ship" of Order:
    description: "Confirmed orders with something left to ship."
    share: role Logistics use, role Admin change
    fields: number, customer, date, total, to_ship
    where: state is Confirmed and to_ship > 0 pcs
    sort: date, number
```

## The file

- `app "Name":` with `description`, `share` (`role <Role> use|change`, `everyone use|change`),
  `filter`s, `let`s and `page`s; or `view "Name" of Entity:` with the options of a view. A file
  holds at most one app and any number of views (different names).
- **Filters**: `filter <name> ["Label"]: <widget> <Entity>.<field path> [default <value>]`
  (widgets: `select`, `multi`, `date_range`, `number_range`, `toggle`, `text`), shown by
  `filters` components (`show: region, period`). Their values are `@region`, `@period` in every
  expression; a view's `select: order` publishes the clicked record as `@order`.
- **`let name = expression`**: a name for an expression, shared by the components (each uses it
  with its own filters applied).
- **Layout**: `row height N:` (grid rows of 40 px) holds components side by side, each with
  `width N` (24ths); `column width N:` stacks its children (`height N`); rows and columns nest;
  `space N` leaves room; `<component> at x, y size w x h` places one anywhere.
- **Components**: `sheet`, `markdown`, `chart`, `kpi`, `filters`, `view`, `button`, `form`,
  then a name and a title: `kpi revenue "Revenue" width 8:`. Each takes the options of its
  settings in the client, under the same names (`source`, `kind`, `label`, `format`, `compare`,
  `fields`, `confirm`…): docs/QDI.md in the repository lists them all, with the exact grammar.
- **A button** runs an action as code calls it: `run: @order.ship(carrier=Road)`,
  `run: Order[42].confirm()`.
- **Sheets**: `tab s1 "Pivot" free 200 x 10:` and its cells: `A1 bold: "Region"`,
  `A2: = Order.count()`, `width A 160`, `merge A1:D1`, `freeze rows 1`.
- What the client stores and has no syntax is kept as `config <key>: <value>`: nothing is lost.

## Custom forms

A form with its own fields, whose submit runs statements, then clears itself:

```python
form quick "Quick order" width 10:
    field number "Number": Code
    field customer: Customer
    field product: Product
    field qty "Quantity": Qty<pcs> where > 0 default 1 pcs
    submit "Order and confirm":
        let o = Order(number=number, customer=customer, date=today())
        OrderLine(order=o, product=product, qty=qty)
        o.confirm()
        o
    after: select Order as placed
    message: f"Order {number}: {qty} of {product.name}"
```

- A field has a type of the language (`?`: optional), a `where` as a field of the code has, a
  `default` (an expression; `@parameters` allowed).
- The statements are checked like an action's body, with the fields and the app's parameters in
  scope. They run **on the server, as the person submitting, in one transaction**, with exactly
  their permissions: the app grants nothing. A `where` not met, a `require` or a `check` that
  fails, a write they may not make: nothing is written, the message shows (at its field when it
  names one).
- `after`: `clear` (when left out), `keep`, `open` (the record the statements give), or
  `select <Entity> as <name>` (published as `@name` for the other components). `message` is
  shown after a success; `confirm` asks before submitting.
- Made in the client too: a form's settings have the mode **Custom**.

## Code apps and data apps

| | App of an interface file | App made in the client |
|---|---|---|
| Where it is | the file, in the repository | the database |
| In production | read-only: "Defined in interfaces/…" — **Duplicate** makes an app of your own | changed by whoever may |
| In `quadra dev` | a change in the client rewrites the file | as always |
| Sharing | roles and everyone in the file; users and teams added in the client | anyone |
| A field renamed | `quadra check` fails at the line (`--fix` applies the new name) | renamed in the database at deploy |

- [`quadra apps pull`](/reference/cli#quadra-apps) writes apps and views made in the client as
  files (then they are the files'), `quadra apps push` installs the files now, `quadra apps new`
  writes a starting file. **Export as .qdi** in an app's ⋯ menu downloads its file.
- When the server starts (and at every version `quadra dev` applies) the files are installed: an
  app or a view added, changed (a new version in its history: `deploy: interfaces/…`), or removed
  when its file is gone. A version whose files do not check is not served.

## In the editor

The VS Code / Cursor extension (`editors/vscode`) colours `.qdi` files and connects them to
`quadra lsp`: diagnostics as you type (a `.qd` change shows at once in the `.qdi` files that it
breaks), completion of the options of a block, of entities, of the fields of the entity a block
is about or of a lambda's record (`Order.where(o => o.`), of `@parameters`; hover and go to
definition of what a file names; find references and rename of a field, an action or an entity
across `.qd` and `.qdi` files.

## Limits

- A cell formula that refers to cells (`A1`, `Sales!B2`) is checked by the sheet when it is
  evaluated, not by `quadra check`.
- `@name` is not read inside an f-string: put it in a `let` or compute it outside.
- A folder made for a directory is renamed, moved or deleted on disk.
