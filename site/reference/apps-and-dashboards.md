# Apps and dashboards

An **app** is a page (or several) where you lay out components on a grid: spreadsheets whose
formulas read the database, text boxes, charts, key figures, global filters, embedded views
and boards, buttons that run actions and forms that create or edit records. Like
[saved views](/reference/views-and-boards), apps are **data**: people make them in the web
client, keep them private or share them, and change them every day, without a deploy. And
like a view, an app never gives data: whoever opens it sees what **they** may see.

## The canvas

**Apps** in the sidebar lists your apps and those shared with you; **New app** makes one
(`#/app/<id>`). An app has pages (tabs). In **Edit** mode:

- the palette adds a component: **Spreadsheet**, **Text** (Markdown), **Filters**, **KPI**,
  **Chart**, **View**, **Button**, **Form** (see [Components](#components));
- a component is moved by dragging its title and resized from its corner, on a grid of 24
  columns and rows of 40 px; what it lands on moves down;
- **Undo**/**Redo** (⌘Z, ⇧⌘Z) step through every change of the layout and the components;
- **Save** keeps the app (`●` marks unsaved changes, kept in your browser meanwhile). If
  someone saved it since you opened it, your save is refused with their change: show theirs, or
  save yours as a copy.

People who may only use the app see it in **View** mode: the spreadsheets compute, nothing
moves.

## Sharing

Exactly as views: an app can be shared with users, [teams](/reference/views-and-boards#teams-or-roles),
roles or everyone, each share **can use** or **can change** (co-owners). Sharing, deleting and
giving the app to someone else stay with its owner and admins. **History** lists every
change, **Duplicate** makes your own copy.

Sharing an app shares its layout and its formulas, never their results: every formula runs as
the person looking at it, with their row rules and hidden fields
([Security](/reference/security)). A salesperson and their manager opening the same app see
different numbers — each the numbers of the orders they may read.

## Folders

**Apps** in the sidebar is a tree: apps in folders, folders in folders, at most **10 deep**.
**+** next to **Apps** makes an app or a folder at the top; a folder's **⋯** menu makes one
inside it, renames it, moves it, shares it or deletes it; an app's **⋯** has **Move to
folder…**. Drag an app or a folder onto a folder to put it there, onto **Apps** to bring it to
the top, or above or below another row to put it in that place (in that folder): the order is
yours, as the order of views is ([`PUT /v1/views/order`](/reference/http-api#views-and-teams)),
favourites first. Which folders are open is remembered per person, in their browser; the
folders of the app you open open by themselves.

- **Names**: a folder's name is a valid directory name (no `/ \ : * ? " < > |`, not ending
  with `.`) and unique among the folders next to it, whatever the case and whoever owns them:
  the tree is one for everyone, like directories on a disk. Two apps may share a name.
- **Sharing a folder shares the folder only, never what it holds**: each app keeps its own
  sharing. Who sees a folder: its owner, those it is shared with, and anyone shown something
  inside it (at any depth) — such a folder is shown only as the way to what it holds (in
  italics), its name and nothing more. So an app shared with you appears in its folder even if
  the folder is not shared with you. A folder **can change** share lets someone rename it and
  put apps and folders in it; putting something in a folder takes the right to change both.
  An app put in your folder by someone else stays theirs and private unless they share it.
- **Delete**: only the folder's owner and admins delete it; what it holds — apps and folders,
  including those you do not see — moves to the folder it was in (each one's history says
  so). If a folder inside has the name of one where it would move, the delete is refused:
  rename one of them first.
- **Moves** are changes like any other: a new version, in the History, `409` when someone
  changed the app or the folder since you read it. A folder never goes into itself or a
  folder it holds.

## Spreadsheets

A spreadsheet component holds a workbook: sheets (tabs at the bottom: add, rename, reorder,
delete), each **fixed** (rows × columns, fitting its box) or **free** (scrolling, up to a
million rows and 16 384 columns, only the cells with something in them are stored).

A cell holds a **value** or a **formula**:

| Typed | Is |
|---|---|
| `42`, `3.5`, `-2` | a number (whole numbers are `Int`) |
| `5.40 EUR`, `3 pcs`, `1.50 EUR/pcs`, `8 h` | an amount, a quantity, a price, a duration — as Quadra literals |
| `10%` | a percent |
| `2026-11-21`, `2026-11-21 09:30` | a date, a date-time |
| `true`, `false` | yes or no |
| `'00123` | text (a leading `'` keeps it text) |
| anything else | text |
| `=…` | a formula |

Styles: bold, italic, underline, strikethrough, font and fill colors, borders, horizontal and
vertical alignment, wrapping, number formats (decimals, thousands, currency, percent, date
patterns), merged cells, column widths, row heights, frozen rows and columns. Copy and paste
(within the sheet, and values from Excel or Google Sheets), fill down (⌘D), inserting and
deleting rows and columns (formulas follow: references move, a deleted cell becomes `#REF!`),
undo and redo.

## Formulas

A formula is a Quadra [expression](/reference/expressions) — the same language, checked by the
same checker, translated to SQL by the same translator as the code — with **cell references**:

| Reference | Is |
|---|---|
| `A1`, `$A$1`, `A$1` | a cell (`$` keeps a row or a column when the formula is copied or filled) |
| `A1:B10` | the cells of a range: a list of those that hold something, row by row |
| `A:C` | whole columns (as far as they hold something) |
| `Sales!B2`, `'Q4 plan'!A1:C3` | a cell or a range of another sheet |
| `A1#` | the whole array the formula in `A1` spilled |

A name the model defines wins over a cell: if an enum has a variant `Q1`, `Q1` is the variant
and `$Q$1` the cell.

```quadra
=A1 * 2
=B2:B20.sum()
=B2:B20.avg()
=Sales!B2 + 'Q4 plan'!B2
=Order.where(o => o.state is Confirmed and o.date >= A1).sum(o => o.total)
=Customer.count(c => c.region.code == A2 and c.active)
=Region.find(r => r.code == A2)?.revenue
=revenue(current.company, DateRange(A1, A2))
=today()
```

Ranges are lists: the [collection methods](/reference/expressions#collection-methods) apply
(`.sum()`, `.count()`, `.avg()`, `.min()`, `.max()`, `.where(x => …)`, `.map(x => …)`,
`.first(n)`…). There are no spreadsheet functions to learn: what the language says is what
the cell says.

**Types are checked as you type.** `=B1 + 3 pcs` when `B1` is `5.40 EUR` is refused with the
checker's message (`cannot add Money<EUR> and Qty<pcs>`), as adding euros to dollars or pieces
to kilograms is; the cell shows `#TYPE`. An empty cell is `none`: `=Z99 ?? 0` gives `0`,
`=Z99 + 1` fails as `none + 1` does in the language.

**Reading the database.** Queries in formulas (`Order.where(…)`, `Customer.count()`,
`Region.find(…)`, [metrics](/reference/metrics)) are answered by the database as the queries of
the code are (in SQL, with the user's row rules), as the user who looks at the sheet:

- the rows of a query are those the user may read: a count, a sum, a group are theirs;
- a field hidden from the user (`hide`) fails with `#HIDDEN`, also inside a query's lambda;
  reading a record they may not read fails with `#NOACCESS`;
- a formula only reads: creating records, running actions, `as user:` and `login()` are
  refused.

A formula that is exactly one call of a metric shows a **certified** mark: its definition is
[certified](/reference/metrics).

**Spill.** A formula whose value is a list fills the cells below it, and a list of tuples (or
of lists) the cells to the right too, as Excel's dynamic arrays do:

```quadra
=Order.where(o => o.state is Confirmed).sort_by(o => o.date).last(100).map(o => (o.number, o.customer.name, o.total))
=Order.group_by(o => o.customer.region).map(g => (g.key.name, g.items.count(), g.items.sum(o => o.total)))
```

If a cell where the result goes holds something, the formula shows `#SPILL` (with the cell in
the way) instead of overwriting it. `A1#` is the whole array; the cells it fills can be
referenced one by one too. At most 10 000 rows are shown: beyond, the last row says how many
more there are (`A1#` still is the whole list).

**Errors** show in the cell, with the message in the formula bar: `#TYPE` (the checker's
message), `#CYCLE` (a circular reference, with its path: `A1 → B1 → A1`), `#REF` (a deleted
cell or an unknown sheet), `#SPILL`, `#HIDDEN`, `#NOACCESS`, `#TIMEOUT` (a request takes at most
20 seconds), `#ERROR` (any other failure: the language's message). A formula using a cell in
error has that error too.

**When values change.** The client sends the workbook to `POST /v1/eval` when it opens and
after each edit (only the cells changed, and the server answers for those and the cells using
them). The server keeps the result of each formula reading the database until a commit changes
one of the entities it reads; when a commit touches them (the live `commit` events), the sheet
is computed again.

**What is computed where.** Everything is computed on the server, as the user. Queries the
database can answer (most filters, counts, sums, groups — see
[Storage](/reference/storage#loading-on-demand)) are SQL; what it cannot (a formula of a
record that is not kept up to date, a function) the engine computes on at most 10 000 rows
of an entity, beyond which the formula asks you to filter first.

### Autocomplete

The formula bar, a cell being edited, the expressions of charts and KPIs and the `{= … }` of
texts suggest what can be written at the cursor, with the types the formula will really have:
after a `.` the fields of a record (through references: `o.customer.region.`), the methods of
a collection with their signature and the fields of its elements, the parts of a value
(`date.month`, a group's `key` and `items`); after `@` the app's filters and selections with
their types; otherwise the parameters of the lambdas you are inside, sheets (`Sales!`,
`'My sheet'!`), entities, enums and their values (those a comparison expects first: `state is
`), functions and certified metrics. While you type the arguments of a call its signature is
shown, the current argument in bold. ↑ ↓ choose, Enter or Tab write, Esc closes the list (a
second Esc cancels the edit), Ctrl/Cmd+Space opens it. Entities you may not read and fields
hidden from you are never suggested. It is `POST /v1/complete`, which reads no data.

## Markdown

A Markdown box shows text: headings, lists, links, tables, images by URL. Raw HTML is shown as
text, never run. `{= expr }` inside the text is evaluated like a cell (the sheets of the
page's first spreadsheet can be referenced: `{= Sheet1!B2 }`, `{= Order.count() }`).

## Components

Each component has its settings (⚙ in Edit mode). What computes something does it on the
server, as the person looking (`POST /v1/eval` for formulas, `POST /v1/query` for rows,
`POST /v1/tx` for writes): an app never grants access to anything.

| Component | What it shows or does |
|---|---|
| **Spreadsheet** | cells, styles and formulas (above) |
| **Text** | Markdown with `{= expr }` values |
| **Filters** | the app's global filters: their widgets, *Clear all*, *Save as my default* |
| **KPI** | a figure from an expression or a cell, compared with the previous period or a target, with a sparkline |
| **Chart** | bar, line, area, pie (donut) or scatter of an expression or of a range of a sheet |
| **View** | a saved view (grid, groups, board) or an entity's rows, with the filters applied |
| **Button** | runs an action or a flow transition of a record, with a form for its parameters |
| **Form** | creates a record of an entity, or edits one |

### Global filters

A **Filters** component defines filters on fields of an entity, through references too:

| Widget | Chooses | In formulas | Type |
|---|---|---|---|
| select | one value of a choice, a flow state, a reference, a text, a yes/no | `@name` | `T?` (`none`: not set) |
| multi | several values | `@name` | `[T]` (`[]`: not set) |
| date range | a period, with presets (this month, last quarter, year to date…) | `@name` | `DateRange` (the whole calendar when not set) |
| number range | a minimum and a maximum | `@name_min`, `@name_max` | `T?` |
| toggle | a yes/no field that holds | `@name` | `Bool` |
| text | a text contained in a field | `@name` | `Text` (`""`: not set) |

The values are the app's, shared by its pages: in the URL
(`#/app/12?f.region=3,5&f.period=2026-01-01..2026-03-31`, a link opens the app filtered),
else your own default (*Save as my default*, kept per user), else the author's default.

Filters reach components two ways.

- **By name, in formulas**: `@region` is a typed parameter of the formulas. It is checked like
  any name: `Order.count(o => o.customer.region in @region)` checks, `@region.first().nope`
  is a type error, `@nope` says which filters there are. Its type comes from the field it
  filters, never from its value: a new value does **not** check the formulas again, it only
  evaluates them again (with the value kept per filter value until a commit changes what they
  read).
- **Bound to an entity**: a KPI, a chart and a view follow, without writing anything, every
  filter of their own entity: with a filter on `Order.customer.region`, `Order.count()` counts
  the orders of the chosen regions and a board of orders shows only them. The component's
  **Filters** settings (`bindings`) apply a filter to another entity through a path —
  `OrderLine` → `order.customer.region` — or turn it off for one. A filter not set holds for
  every row. Spreadsheets and texts are not bound by default (their formulas say what they
  filter with `@name`); their settings can bind them too.

A bound filter becomes a condition of the queries (`Order.where(…)` in the formulas, `filters`
of `/v1/query`) that is the same whatever the values: it is checked once and translated to
SQL with the rest of the query.

### The selected record

A **View** may publish the record selected in it (click a row or a card; click again to
clear): its setting *Selected record as* names it, e.g. `order`. It is then a parameter like a
filter: `@order` (an `Order?`) in formulas, the target of a button or a form, a filter of
another view (*lines of the selected order*: bind `OrderLine` → `order` to `@order`).

### Editing through a view

A **View** is read-only unless its author enables editing in its settings (*Editing*, stored
as `config.edit`):

| Option | What its users may do |
|---|---|
| *Edit cells inline* (`cells`) | change cells of the grid (and of the rows a group leads to), with the entity page's editors and checks |
| *Add rows* (`add`) | add rows at the end of a grid (*+ Row*, or the last empty row) |
| *Move cards between columns* (`move`) | drag a card of a board to another column, or move it with Space / ⇄: on a flow it runs the transition, with every check |

Changes are drafts (yellow) until *Save* in the small bar above the grid, or ⌘S while the
grid has the focus (⌘S elsewhere saves the app in Edit mode); *Discard* drops them. They are
the entity page's: a value the field refuses is marked red with the message; if someone else
changed one of those records since it was read, nothing is saved and the grid shows who
changed what. A board's cards stay where they are without `move` (they are still selected by
a click). There is no option to delete rows: records are not deleted in Quadra — to cancel or
close one, add a **Button** running its action.

Enabling editing grants nothing: each change is a `POST /v1/tx` of the person making it. A
field they may not change, a formula, a frozen record or a field of another record
(`customer.region`) stays read-only, and the reason shows under the grid when the cell is
selected; a record their permissions do not let them change (a row rule) is refused by the
server when they save, with its message. The server checks the shape of `edit` when the app
is saved (only `cells`, `add`, `move`, as `true` or `false`; `add` on a grid, `move` on a
board; `delete` is refused with the reason above).

### Charts

The data is an expression giving a list — its first column the labels, the others the series
(named by the tuple's names) — or a range of a spreadsheet of the page (first row: the series'
names, first column: the labels), followed as the sheet computes:

```quadra
Order.where(o => o.state in {Confirmed, Shipped})
    .group_by(o => o.date.start_of_month())
    .map(g => (month: g.key, revenue: g.items.sum(o => o.total)))
```

Bar (stacked, horizontal), line, area (stacked), pie (as a donut) and scatter, with axes,
tooltips (mouse and keyboard), a legend that hides a series, colours readable in light and
dark mode, a table view, and at most *max points* (500 by default: the chart says when it shows
fewer than the result has).

### KPIs

A figure from an expression or a cell, with a number format, optionally compared with the
**previous period** (the same expression with a date-range filter moved back by its own length:
a month to the month before, a quarter to the quarter before) or a **target** (an expression),
showing the change and its colour (*higher is better* or not), and a sparkline (an expression
giving numbers or `(label, number)` pairs). A KPI that is one call of a certified
[`metric`](/reference/metrics) carries the certified mark.

### Buttons

A button runs an **action** or a **flow transition** of a record (the list comes from the
model as you may see it): the record from a filter or a selected record (`@order`), a fixed
id, or a cell. Parameters are asked by a form made from the action's signature (dates,
numbers, amounts, references with search), with their defaults; an optional confirmation;
errors from the server are shown at the parameter they name. The button is disabled, with the
reason, when you may not run the action (your role), when the record is not in a state the
transition starts from (*only from Draft (it is Confirmed)*), or when nothing is selected.
After it runs, every component reading what changed computes again.

### Forms

A form **creates** a record of an entity or **edits** one (from a selected record, a filter,
an id, a cell), with the fields you choose in sections of 1 to 3 columns. Each field gets the
editor of the grid and the record panel (texts, numbers, amounts with their currency, dates,
choices, references with search, addresses; lists read-only). Defaults may come from filters.
Fields hidden from you are not shown, fields you may not change are read-only; the server's
messages appear at their field; if someone changed the record since you opened it, the form
says who and what, and lets you reload theirs or apply yours again on top.

### Example: the Sales cockpit

`examples/stress/apps/sales-cockpit.json` is an app to import (`quadra db views import
examples/stress/apps/sales-cockpit.json examples/stress --url …`, shared with everyone):
filters on region, period and state; KPIs (revenue against the previous period with a
sparkline, orders, open orders, the average order against a target); the monthly revenue; the
top 10 customers; the board of orders by state, whose selected card is `@order`; buttons
confirming and cancelling the selected order; a form creating an order; and a sheet with a
region × state pivot (`group_by` spilled) bound to the filters. A salesperson opening it sees
the same app with their own orders in every component, and may confirm but not ship.

## Excel export

**Export to Excel** in a spreadsheet's menu downloads the workbook as an `.xlsx` file
(`GET /v1/apps/{id}/export.xlsx?component=…&scope=…`), computed as you with the filters as they
are set: what you may not read is not in it. A chart's menu exports its data (labels and
series), a KPI's its value, its comparison and its sparkline's points. Every sheet, with its name; numbers as numbers, dates as dates, amounts as numbers with
their currency in the number format; bold, italic, colors, fills, borders, alignment, number
formats, merged cells, widths, heights, frozen panes. A formula is exported as its value, with
its Quadra text in a note of the cell.

## Where apps are kept

An app can also be a file of the project, checked with the code and deployed with it: see
[Interface files](/reference/interfaces).

Apps are kept with the saved views (`_quadra_views`, `kind = "app"`; folders `kind =
"folder"`, the folder of each in `parent_id`), with the same history, sharing and soft delete,
and move between databases with them, the tree included (`quadra db views export|import`,
[CLI](/reference/cli#quadra-db-views): folders are found again by name in their place, the
missing ones made). The HTTP API is
[`/v1/views`](/reference/http-api#views-and-teams) with `kind: "app"` or `kind: "folder"`,
[`POST /v1/eval`](/reference/http-api#apps-and-formulas) and the export.

In the [sandbox](/sandbox) of this site there is no server: apps are not available there
(saved views are kept in the browser).

A deploy does not change apps: a formula using a field that no longer exists shows the
checker's error in its cell until it is fixed (renames of migrations are not applied to
formulas yet).
