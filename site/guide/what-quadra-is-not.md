---
title: What Quadra is not
description: The boundaries of Quadra, the language, the engine, the studio and the cloud. One hundred things Quadra does not do, and what to use instead.
outline: 2
---

# What Quadra is not

Saying what a tool will not do is as useful as saying what it does. It tells you, before you
start, whether Quadra fits your problem, and it tells everyone who asks for a feature where the
answer is already written. This page lists one hundred of those answers.

**Quadra is a language for writing the model of a business (its data, money, rules,
processes, permissions and certified numbers) so that the books always balance and every
number can explain where it comes from.** The database, the API, the interface and the hosting
are derived from that model. What does not serve that sentence does not go in.

Every item says which kind of answer it is:

| Mark | Meaning |
|---|---|
| <Badge type="tip" text="already so" /> | Quadra already works this way today. The item makes it a promise. |
| <Badge type="info" text="consequence" /> | It follows from a choice already made (determinism, the hashed history, one writer per database, no I/O in formulas). Saying yes would break that choice. |
| <Badge type="warning" text="proposal" /> | A position we intend to hold, still open to discussion. |

## Five questions before any feature

When a request comes in, ask these in order. One "no" is enough to stop.

1. **Does it help people who write business software?** Orders, invoices, stock, production,
   accounting, approvals. A website, a game or a scientific model: no.
2. **Does the engine stay deterministic?** Same data, same code, same result, also when a year
   of history is replayed. A formula that reads the clock, the network, a file or an AI model: no.
3. **Can the compiler still see the whole model?** Quadra's checks (cycles, unreachable states,
   cascades, currencies) exist because the model is declared. An escape hatch the compiler
   cannot read switches them off for everyone.
4. **Can it be done with what already exists?** An entity, a `trait`, an `extend`, a reaction,
   a library module, the API. If so, the answer is an example, not a feature.
5. **Does it work the same outside Quadra Cloud?** The language does not depend on the cloud. A
   language feature that exists only in the cloud is a no.

Every yes is paid for forever: in the grammar, the type checker, the engine, the formatter, the
language server, the studio, the documentation and the tests, on SQLite and on PostgreSQL. The
default answer is no.

## Identity

1. **Not a general-purpose language.** <Badge type="tip" text="already so" /> Quadra does not
   compete with Rust, Go or Python. A program that is not about records, money, states and
   permissions is written elsewhere and talks to Quadra through the [HTTP API](/reference/http-api).
2. **Not a drag-and-drop builder that hides the model.** <Badge type="tip" text="already so" />
   Screens can be made in the app and an AI assistant can write the rules, but the model is
   always text: checked by a compiler, reviewed and versioned in git, tested.
3. **Not a ready-made ERP.** <Badge type="tip" text="already so" /> Quadra is the language you
   write an ERP in. The [extended example](https://github.com/adileo/quadra/tree/main/examples/extended)
   is an example, not a product to configure. National accounting, e-invoicing formats, lot
   tracking are modules written in Quadra, not features of the engine.
4. **Not an ORM.** <Badge type="tip" text="already so" /> Quadra does not map classes onto
   existing tables: the model is the source, and the tables are projections of the history
   ([Storage](/reference/storage)). An existing database is reached with an import or an
   integration.
5. **Not a web framework.** <Badge type="tip" text="already so" /> No routes, controllers,
   middleware or templates written by hand. The API and the studio come from the model; a
   website uses a web framework and calls the API.
6. **Not a database.** <Badge type="tip" text="already so" /> Quadra stores its data in SQLite
   or PostgreSQL. It is not sold as a storage engine and does not replace a database for
   workloads that are not business records.
7. **Not a spreadsheet engine.** <Badge type="tip" text="already so" /> Apps have cells and
   formulas, but the formulas are typed Quadra expressions and every write goes through the
   rules ([Apps and dashboards](/reference/apps-and-dashboards)). No free cells that save data
   unchecked, no macros.
8. **Not a BPMN or generic workflow engine.** <Badge type="tip" text="already so" /> Processes
   are `flow`, `action`, reactions and events on real records
   ([Flows and actions](/reference/flows-and-actions)). No executable diagrams kept apart from
   the data.
9. **Not an integration platform.** <Badge type="info" text="consequence" /> Quadra talks to the
   outside with `emit` and `outbound`, and receives recorded messages
   ([Variables and connections](/reference/connections)). Ready-made connectors are a few library
   modules, not a catalogue of hundreds of apps.
10. **Not a BI tool or a data warehouse.** <Badge type="info" text="consequence" /> Certified
    metrics, views, pivots and KPIs are there, because they are needed to work. Analysis over
    years of data, cubes and joins with outside sources belong to the tools made for them,
    which read Quadra's SQL tables or its API.
11. **Not a CMS, a shop or a website builder.** <Badge type="tip" text="already so" /> Apps
    show no HTML written by their users and sit behind a sign-in. Storefronts, blogs and public
    pages stay outside.
12. **Not a tool for every trade.** <Badge type="warning" text="proposal" /> The audience is the
    people who write business software and the people who must trust its numbers. Orders,
    invoices and stock set the priorities, not generic CRUD apps.

## Syntax

13. **No two ways to write the same thing.** <Badge type="warning" text="proposal" /> A new form
    must remove something, or cover a case that cannot be written today. No sugar for comfort.
14. **No braces or `end` for blocks.** <Badge type="tip" text="already so" /> Indentation
    delimits blocks, spaces only; a tab is an error ([Syntax](/reference/syntax)). There will
    be no alternative brace syntax.
15. **No formatter options.** <Badge type="warning" text="proposal" /> `quadra fmt` has one
    output. No line width, quote style or alignment settings: one style, nothing to argue about
    in reviews.
16. **No macros and no metaprogramming.** <Badge type="info" text="consequence" /> No code that
    writes code, no `derive`, no reflection at run time. The compiler, and the person reviewing,
    read the model as it is written.
17. **No user-defined operators and no overloading.** <Badge type="warning" text="proposal" />
    `+` on `Money<EUR>` does what [Types](/reference/types#arithmetic) says, always. Nobody
    redefines `+` on an entity.
18. **No modulo, exponents, hexadecimal or bit operators.** <Badge type="tip" text="already so" />
    `%` is a percentage. Business software does not need them, and each would take a symbol.
19. **No `while` or `loop`.** <Badge type="tip" text="already so" /> `for` iterates over finite
    collections. It is a termination guarantee worth keeping; the rest is covered by the
    engine's time and depth limits.
20. **No `try`/`catch`.** <Badge type="tip" text="already so" /> A failure (`fail`, `require`, a
    broken rule) undoes the whole transaction. An alternative is written before, with `??` or
    `if`. Half-done transactions do not exist.
21. **No `async`/`await`, threads or coroutines.** <Badge type="info" text="consequence" />
    Concurrency belongs to the engine (parallel reads, optimistic concurrency), never to the
    user's code.
22. **No keywords in other languages.** <Badge type="warning" text="proposal" /> Keywords are
    English and stay so. Names, messages and labels can be in any language, and identifiers
    accept non-ASCII letters.
23. **No reserving of common words.** <Badge type="tip" text="already so" /> Reserved keywords
    are few; the others are contextual, so `key`, `to` and `parent` stay usable as field names.
    A new reserved word costs every existing model, so it comes only with a new edition.
24. **No import aliases, whole-module imports or import cycles.** <Badge type="tip" text="already so" />
    Every name is imported explicitly and modules are layered
    ([Projects and modules](/reference/projects-and-modules#imports)). A cycle is solved with
    layers or with `extend`.
25. **No implicit scope.** <Badge type="tip" text="already so" /> Inside a lambda you use its
    parameter: no `it`, no `with`, no names that change meaning with the context.
26. **No `null`.** <Badge type="tip" text="already so" /> There is `none`, only in optional types
    `T?`, and the compiler makes you handle it (`?.`, `??`, `if let`, `required when`).
27. **No anonymous unions.** <Badge type="tip" text="already so" /> `Int | Text` is refused: use
    an `enum` or a `trait`. Every set of cases has a name, and a `match` must cover them all.
28. **No SQL inside the language.** <Badge type="info" text="consequence" /> No query strings,
    no raw SQL. Queries are `where`, `group_by` and `sum`, translated by the engine. A SQL
    escape hatch would bypass permissions, history and the dependency analysis.
29. **No free-form annotations that change behaviour.** <Badge type="warning" text="proposal" />
    No `@cache`, `@async` or `@index`. What changes meaning is documented, checked syntax; the
    rest lives in `quadra.toml` or does not exist.
30. **No constructs for a single customer.** <Badge type="warning" text="proposal" /> Syntax
    that one company needs is a module of that company.
31. **No `saga` construct.** <Badge type="info" text="consequence" /> A saga is a record with a
    `Pending` state, an `emit`, reactions to the outcomes and a deadline on the clock. The parts
    are enough.
32. **No construct per protocol.** <Badge type="tip" text="already so" /> Protocols,
    authentication and signatures are kinds of connection in `quadra.toml`, not new syntax.
33. **No words added for style.** <Badge type="warning" text="proposal" /> `implies`, `becomes`
    and `frozen` exist because they state a business concept that would otherwise be written
    badly. A new word must pass the same test.
34. **No silent changes of meaning.** <Badge type="tip" text="already so" /> The project's
    `edition` in `quadra.toml` fixes how the code is read. A breaking change comes with a new
    edition, never with an option or a flag.

## Types and semantics

35. **No floating point.** <Badge type="tip" text="already so" /> Only exact decimals. There will
    be no `Float` for scientific calculations: that is not Quadra's job.
36. **No automatic currency conversion.** <Badge type="tip" text="already so" /> `EUR + USD`
    does not compile. A conversion is written with `convert`, with a rate or a date, and the
    result is `Exact` until it is rounded ([Types](/reference/types#conversions)).
37. **No hidden rounding.** <Badge type="tip" text="already so" /> An unrounded amount cannot be
    stored. "Round it automatically at the end" means hiding where the cents come from.
38. **No mixing of incompatible units.** <Badge type="tip" text="already so" /> `kg + pcs` does
    not compile, and there is no lenient mode.
39. **No research-grade type system.** <Badge type="warning" text="proposal" /> No user-written
    generics, general dependent types or higher-kinded types. Type parameters exist where the
    domain needs them (`Money<C>`, `Qty<U>`, `Price<C, U>`) and stay there.
40. **No inheritance between entities.** <Badge type="tip" text="already so" /> There are
    [traits](/reference/entities#trait) (shared fields, and a type for references) and `extend`.
    No class hierarchies, no `super`, no overrides.
41. **No format validation by type name.** <Badge type="tip" text="already so" /> `Email`,
    `VatNumber` and `Code` are text with an intent; the format is checked with `where`. The
    VAT number rules of every country belong in modules, not in the engine.
42. **No dates with a hidden time zone.** <Badge type="tip" text="already so" /> A `Date` is a
    day, a `DateTime` an instant in UTC, the project's zone is used only in conversions and any
    other zone is written out ([Types](/reference/types#dates-times-and-time-zones)).
43. **No clock in formulas.** <Badge type="tip" text="already so" /> `now()` in a formula is an
    error. What depends on time is a `default`, an action, a reaction or a rule re-evaluated by
    the sweeps.
44. **No side effects in expressions.** <Badge type="tip" text="already so" /> Formulas, checks,
    functions and permission rules are pure. Effects belong to actions, reactions and tests.
45. **No I/O from the code.** <Badge type="tip" text="already so" /> No formula or action calls
    the network or reads a file. The outside world comes in as a recorded message and goes out
    as a recorded event.
46. **No reading of secrets.** <Badge type="tip" text="already so" /> A secret is only handed to
    a connection: the code knows its name, never its value
    ([Variables and connections](/reference/connections#variables)).
47. **No arbitrary URLs or hosts.** <Badge type="tip" text="already so" /> The destination is a
    declared connection; the code chooses only the path. No SSRF, no exfiltration.
48. **No AI models inside formulas or rules.** <Badge type="info" text="consequence" /> An AI is
    not deterministic, and a certified number cannot depend on one. An AI can write Quadra code,
    read the data through the API and propose changes like anyone else; a result that enters the
    data enters as a recorded message.
49. **No randomness in the engine.** <Badge type="warning" text="proposal" /> No `random()` in
    formulas and rules. When a case truly needs it (a discount code), the value is made outside
    or in an action, and ends up in the history.
50. **No history as a value in expressions.** <Badge type="tip" text="already so" />
    `rec.history` and `rec.as_of(…)` are compile errors: the history is read with `why()` and
    the API. A formula that reads the past could not be kept up to date.
51. **No endless cascades.** <Badge type="tip" text="already so" /> A reaction that can trigger
    itself is refused until it is declared `converges`, and at run time 20 rounds stop the
    transaction. The limit will not be raised for one case: the model is rewritten.
52. **No states written by hand.** <Badge type="tip" text="already so" /> A state changes only
    through the actions of its flow. There is no "allow direct assignment for admins".

## Data, history and storage

53. **No deleting or rewriting of history.** <Badge type="tip" text="already so" /> Every value
    written is kept, and the hash chain proves it. Erasing personal data will work by
    destroying their encryption keys, not by removing rows.
54. **No `UPDATE` by hand on the tables.** <Badge type="tip" text="already so" /> The tables are
    projections, and `quadra db verify` finds who touched them. Bulk corrections are
    [`change`](/reference/migrations-and-changes#changes) blocks: reason, preview, approval,
    revert.
55. **Not a database other programs write to.** <Badge type="info" text="consequence" /> Other
    tools read the SQL tables; they write only through the API, which applies rules and
    permissions.
56. **No triggers, stored procedures or user views in the database.** <Badge type="info" text="consequence" />
    All the logic lives in the model, so it holds from every entry point and can be tested.
57. **No schema changes at run time by end users.** <Badge type="warning" text="proposal" /> No
    custom fields added from the studio in production, no entity-attribute-value tables. A new
    field is code: `extend`, review, migration, deploy. That is the price of types, and it is
    worth paying.
58. **No soft delete by convention.** <Badge type="warning" text="proposal" /> The life of a
    record is its `flow` (`Cancelled`, `Archived`), and the history keeps everything. No
    implicit `deleted` column.
59. **No transactions across databases.** <Badge type="info" text="consequence" /> A project's
    environment is one database with one writer. Two of them talk through events and messages,
    never through a distributed commit.
60. **Not multi-master, not offline-first.** <Badge type="info" text="consequence" /> No CRDTs,
    no writable replicas syncing from a phone. There is one writer, and that is why the books
    balance.
61. **No pessimistic locks in the code.** <Badge type="tip" text="already so" /> Concurrency is
    optimistic: a commit on data that others changed is refused with who, what and when
    ([Storage](/reference/storage#optimistic-concurrency)). No `select for update` in the
    language.
62. **Not a file store.** <Badge type="warning" text="proposal" /> Attachments, within what
    business software needs. Videos, backups and gigabyte datasets go to an object store, and the
    record keeps a reference.
63. **Not a time-series or telemetry database.** <Badge type="warning" text="proposal" /> Sensor
    readings every second, application logs and clickstreams: no. Business software receives the
    summaries (consumption per shift, a machine stop), not the raw stream.
64. **Not a search engine.** <Badge type="tip" text="already so" /> Word search on text fields,
    yes ([Full-text search](/reference/storage#full-text-search)). Semantic search, ranking,
    synonyms and vectors: no.
65. **No storage engines beyond SQLite and PostgreSQL.** <Badge type="warning" text="proposal" />
    No MySQL, SQL Server or Oracle as Quadra's own storage. Reading from and writing to them as
    outside systems, yes, through integrations.
66. **No schema patched by hand per installation.** <Badge type="tip" text="already so" /> The
    schema comes from the code. Every installation of the same code has the same tables.
67. **No migrations that hope.** <Badge type="tip" text="already so" /> A
    [migration](/reference/migrations-and-changes#migrations) checks the new rules on the real
    data, with a dry run, provisional values and verification.
68. **No "trust me" numbers.** <Badge type="tip" text="already so" /> A certified
    [metric](/reference/metrics) cannot change meaning quietly: a changed definition fails
    `quadra check` until someone approves it again.

## Logic, permissions and integrations

69. **No permissions in endpoints.** <Badge type="tip" text="already so" /> Permissions are
    [`role`](/reference/security) rules on rows and fields, valid everywhere. No "only admins see
    this endpoint" outside the model.
70. **No per-record sharing by hand.** <Badge type="warning" text="proposal" /> No file-sharing
    style access lists. To share a record with a person, model it: an entity of shares and a
    `read` rule.
71. **No data given away by sharing an app or a view.** <Badge type="tip" text="already so" />
    Sharing an app shares its layout and formulas; everyone sees what they may see.
72. **Not an identity provider.** <Badge type="warning" text="proposal" /> Quadra manages the
    users and roles of its apps, sign-in with emailed codes and API tokens. It does not become
    the identity provider of other systems.
73. **No rules edited by business users at run time.** <Badge type="warning" text="proposal" />
    Rules are code, with tests and review. What must change often (a price list, a threshold) is
    data, and a rule reads it.
74. **Not a message broker.** <Badge type="info" text="consequence" /> The outbox guarantees the
    delivery of a project's events. Topics, consumer groups and retention policies between
    other systems belong to a queue or a broker.
75. **Not an ETL tool.** <Badge type="warning" text="proposal" /> Bulk import, incoming files and
    typed external formats, yes. Transformation pipelines between systems that do not touch the
    model: no.
76. **No code in other languages.** <Badge type="info" text="consequence" /> No JavaScript,
    Python or Lua inside actions, no user WebAssembly in the engine. The way out is an outside
    program that uses the API and reads the events.
77. **No engine plugins.** <Badge type="warning" text="proposal" /> The engine loads no native
    extensions. The command line runs outside commands named `quadra-<name>`, as git does, and
    they stay outside the engine ([CLI](/reference/cli)).
78. **No synchronous calls inside a transaction.** <Badge type="info" text="consequence" /> An
    action never waits for an HTTP reply. It emits an event, the event is delivered after the
    commit, and the outcome comes back as a new transaction.
79. **No "exactly once" promise to the outside.** <Badge type="info" text="consequence" /> No
    committed event is ever lost, and duplicates carry the same idempotency key. The rest is up
    to the receiver, and we say so.
80. **Not an email marketing tool.** <Badge type="warning" text="proposal" /> Transactional email
    (an order, an invoice, a reminder), yes. Campaigns, lists, A/B tests and open tracking: no.
81. **No documents rendered by a browser.** <Badge type="info" text="consequence" /> Printed
    documents must come out identical byte for byte when regenerated, so they are produced by an
    embedded typesetting engine, never by a headless browser turning HTML into PDF.
82. **No WYSIWYG print designer as the source.** <Badge type="warning" text="proposal" />
    Documents are files of the project, checked by the compiler. A visual editor may come on top,
    but the format stays text.

## The studio

83. **Not a free-form interface builder.** <Badge type="tip" text="already so" /> The studio
    shows the model: grids, boards, records, views and apps made of known components. No pages
    drawn pixel by pixel.
84. **No user HTML, CSS or JavaScript.** <Badge type="tip" text="already so" /> Not in apps, not
    in fields, not in themes. A custom interface is built outside, on the API.
85. **No second kind of app.** <Badge type="tip" text="already so" /> An app made in the studio
    and an [interface file](/reference/interfaces) are the same thing, checked with the code.
86. **No business logic in the interface.** <Badge type="info" text="consequence" /> A button
    runs an action, a form creates a record: the rules are in the model. No validation that
    exists only in a form.
87. **No native mobile apps from Quadra.** <Badge type="warning" text="proposal" /> The studio is
    a web app and must work well on small screens. A native app is built on the API, with its
    own release cycle.
88. **No offline mode.** <Badge type="info" text="consequence" /> See item 60. A warehouse without
    coverage uses a device that queues its work and sends it to the API when the network is back;
    the engine does not change.
89. **Not a full IDE.** <Badge type="warning" text="proposal" /> The studio's editor is for quick
    fixes and trials. Serious work happens in your editor with the
    [extension and language server](/guide/installation). No visual debugger, extension manager
    or terminal in the studio.
90. **No studio per industry.** <Badge type="warning" text="proposal" /> No construction studio,
    no healthcare studio. The industry is in the model; the studio is one.

## Quadra Cloud

91. **Not a generic hosting platform.** <Badge type="tip" text="already so" /> Quadra Cloud runs
    Quadra projects only: no container images, no services in other languages, no separate
    databases.
92. **No language features that exist only in the cloud.** <Badge type="tip" text="already so" />
    The language does not depend on the cloud and runs the same when you host it yourself. The
    cloud adds hosting, limits, metering and delivery, not meaning.
93. **No container per customer.** <Badge type="tip" text="already so" /> Shared processes run
    the projects, and idle ones sleep in object storage at no cost.
94. **No lock-in to a provider.** <Badge type="tip" text="already so" /> The platform runs on
    plain machines with docker compose, and a project's data is a database file and its log,
    which can move to any machine, or to your own servers.
95. **No access to the database file or the machine.** <Badge type="tip" text="already so" /> No
    shell on the servers, no download of a live database by your code. Exports and the console go
    through the API, with the permissions of whoever asks.
96. **No cron jobs with arbitrary code.** <Badge type="tip" text="already so" /> What depends on
    time is rules and reactions that the engine re-evaluates when they fall due. No "script that
    runs every night".
97. **No unlimited resources.** <Badge type="tip" text="already so" /> Every plan has limits per
    request, per job and per size. They are not raised for a slow model: the query is rewritten.
98. **No management of the customer's infrastructure.** <Badge type="warning" text="proposal" />
    An on-premise agent will reach systems behind a firewall. It will not install, update or
    monitor the customer's servers.
99. **No custom builds for one customer.** <Badge type="warning" text="proposal" /> Every
    project runs the same released engine. A fix or a feature one customer needs goes into a
    release for everyone, or into that customer's own code.
100. **Not a consulting service.** <Badge type="warning" text="proposal" /> Quadra Cloud hosts
     projects; it does not write them. Building your system is the work of your team, of a
     software house, or of a partner.

## Not "no", but "not yet"

These are **in scope**, on the roadmap, even if they are not there today:

- a PostgreSQL class for very large projects in the cloud;
- custom domains for apps in the cloud;
- files over SFTP, databases of other management systems, incoming email, and an on-premise
  agent;
- printed documents, labels and signed PDF and XML;
- signed commits and erasure of personal data;
- an interactive console to explore the data.

And for whoever asks for something on the list above, these roads stay open: the
[HTTP API](/reference/http-api), the live stream, the events, the SQL tables for reading,
`quadra-<name>` commands, and Quadra modules published by others. A "no" to a feature is
almost never "it can't be done": it is "it is done outside the engine".

## How this page changes

- Removing an item means saying which guarantee of Quadra gets weaker, and why it is worth it.
  A customer's request alone is not enough.
- Adding one is easy: a case where we said no twice.
- Items marked <Badge type="warning" text="proposal" /> are confirmed or removed one by one;
  until then, they are a direction, not a rule.
