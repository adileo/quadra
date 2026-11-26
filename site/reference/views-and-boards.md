# Views and boards

A Quadra project has two layers. **Code** — the `.qd` files: entities, rules, flows, roles,
metrics — is versioned in git, tested and deployed. **Data** is what people create while they
work. Saved views and boards are data: people make them in the web client, keep them private
or share them, and change them every day, without a deploy.

## Saved views

Every sheet of the web client can be saved as a view: its columns (and their widths and
order), the filter (`where`, a Quadra expression), the full-text search, the sort, personal
columns (`★`, Quadra expressions), color rules, groups or a pivot, and a board. A view is of
one of three types:

| Type | Shows |
|---|---|
| `grid` | the rows |
| `groups` | the rows grouped by one or two keys, or a pivot ([groups](/reference/http-api#groups-and-pivots)) |
| `kanban` | a board: one column per value of a field, the rows as cards (below) |

The **View** menu above the sheet lists the **Standard** view (every field; what you change
there stays in your browser), your favourites, your views and the views shared with you.

| Action | Who |
|---|---|
| **Save view** | its owner, co-owners and admins |
| **Save as…** a new view (private) | anyone who can open it |
| **Share…** and **Delete** | its owner and admins |
| Rename, Duplicate, History | Rename: those who may change it; the others: anyone who can open it |
| ★ favourite, **Open it first** (your default for the entity) | each person for themselves |

Starred (★ favourite) views are also listed in the sidebar under their entity: the arrow next
to the entity shows or hides them (remembered per person, in their browser), a click opens the
entity with that view (`#/Order?v=12`), and dragging one above or below another reorders them
(the other views keep their places in the **View** menu). Starring or unstarring a view, here
or in another tab, updates the sidebar at once.

Unsaved changes to a saved view are kept in your browser (the view shows `●`); **Revert**
drops them. Expressions are checked when a view is saved, with the messages of the editor: a
filter that does not type check, a path to a field that does not exist, or a filter that is not
yes or no is refused.

### Sharing never gives data

A view is a configuration, never a copy of data. Whoever opens it runs it **as themselves**:
their row rules and hidden fields apply as on any sheet ([Security](/reference/security)). Two
people opening the same view may see different rows — a salesperson their own orders, a
manager those of their region — and a field hidden from someone stays hidden in every view.

A view can be shared with:

| With | Who sees it |
|---|---|
| a user | that user |
| a team | the team's members, as long as they are members |
| a role | the users who have the role, directly or through a role that extends it (`role SalesManager is Salesperson`: a view shared with `Salesperson` is shown to sales managers too) |
| everyone | every user |

Each share is either **can use** (open it, save it as their own copy) or **can change**
(co-owners: they save changes too; sharing and deleting stay with the owner). A view of an
entity a user may not read is not listed to them.

**Admins** — the users who may edit `User.roles` (with no `role` declared yet, everyone) — may
open, change, share and delete every view, and manage every team. Their own list shows the
views they own or that are shared with them, like anybody else's.

### Teams or roles?

| | Role | Team |
|---|---|---|
| What | a function, with its permissions | a working group |
| Where | the code (`role Salesperson:`), deployed | data, managed in the app (**Teams**) |
| Who changes it | developers, through a deploy | admins, and the team's owners |
| Share a view with it when | it is the official view of that function (“the board every salesperson works from”) | a group of people works on something together (“the North desk's open orders”) |

Admins create teams; a team's owners (whoever created it, unless other owners are named) add
and remove members. Removing someone from a team removes the views shared with that team from
their list at once; deleting a team stops all its shares.

## Boards

A board shows the rows of a view as cards, one column per value of a field:

- the states of a **flow** field (in the model's order) — moving a card runs the transition;
- the variants of a **choice** (`enum`), or **yes/no** — moving a card changes the field;
- the records of a **reference** with few targets (at most 40 columns) — moving a card
  changes the reference.

A column for the rows without a value is added when the field is optional (or some rows have
none). In **Board…** you choose the field, the card's title (the record's name by default),
the fields shown on the cards (rendered as in the grid: amounts, dates, references as links,
choices as chips), a total per column (a sum), the order of the cards, the columns shown, and a
work-in-progress limit per column (shown in red when exceeded; never enforced). The view's
filter, search, groups drilled into and color rules apply to every column. A column header can
be collapsed. Clicking a card opens the record.

### Moving a card

Dropping a card in another column is a transaction like any other (`POST /v1/tx` with the
`base` the board was read at), with every check of the model:

- on a **flow**, the action of the transition between the two states runs
  (`Draft -> Confirmed by confirm`: `confirm`). When several actions lead there, you choose
  one. A transition whose action needs parameters is done from the record;
- on a **plain field**, the field is updated.

While a card is dragged, the columns it may not go to are greyed out, with the reason on
hover: no transition between the two states, an action the user may not run on that record
(their row rules), a frozen record, a field they may not edit. What only the transaction can
tell — a `require` of the action, a `when`/`unless` guard, a check, a conflict with someone
else's change — refuses the move: the card goes back and the model's message is shown
(`O-1042 was not moved: The order has no lines`).

### On large data

A board never loads every card. The count (and the total) of every column is one group query
of the database; each column is a page of 30 cards (in the view's order), and the next page is
read as the column scrolls. When the counts cannot be grouped (a filter the database cannot
apply on a very large entity), each column shows its own total, an estimate beyond 10 000 rows
(`~`). Commits by others refresh the board: a card someone moved moves.

## What happens on deploy

Views are not code, so a deploy never deletes them; they follow the model:

- a migration's `rename Entity.old -> Entity.new` renames the field in every view that uses it
  — its columns, sort, groups, board, and inside its expressions (`customer.name.starts_with("A")`
  becomes `customer.title.starts_with("A")`) — recorded in the view's history as
  `deploy: renamed customer.name → customer.title`;
- a removed field, an entity that no longer exists, or a type change that breaks an
  expression marks the view as **broken**, with the checker's messages. The view still opens:
  columns that no longer exist are hidden, and a banner lists what no longer fits. **Remove
  what no longer exists** drops those parts; fix the rest and **Save view**.

Views are checked against the new model when the server starts and after each deploy from the
browser or `quadra dev`.

## Where views are kept

Views, teams, favourites and defaults are kept in system tables of the database
(`_quadra_views`, `_quadra_view_shares`, `_quadra_view_prefs`, `_quadra_teams`,
`_quadra_team_members`), not in the model: they never appear among the entities. Every change
is one transaction recorded in `_quadra_view_log` (who, when, what changed, the state after it:
**History** in the view's menu). Two people changing a view at the same time do not overwrite
each other: the second save is refused (`409`) with the first one's change, and can be saved as
a new view instead. A deleted view is marked deleted, with its history.

`quadra db views export views.json` writes the views and teams of a database to a file, users
by login and teams by name; `quadra db views import views.json` loads them into another
database ([CLI](/reference/cli#quadra-db-views)). The HTTP API is
[`/v1/views` and `/v1/teams`](/reference/http-api#views-and-teams).

Sheets changed in a browser before views were kept by the server are copied, the first time
the user signs in, into private views named “My view” (opened first for that entity); the
browser's copy stays as the Standard view.

In the [sandbox](/sandbox) of this site the engine runs in the page without a database: views
are kept in the browser, private, and there are no teams.

## From a view to code

Apps — pages of spreadsheets, text and (soon) charts, filters and forms — are kept and shared
the same way: see [Apps and dashboards](/reference/apps-and-dashboards).

A view stays data. When one becomes an official report of the business — the same for
everyone, reviewed, tested and versioned — it belongs in the code; turning a saved view into a
definition of the model, shipped by a deploy, is a later step.
