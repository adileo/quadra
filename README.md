# Quadra

A language for business applications. You describe the data, the rules, the processes, the
permissions and the certified metrics of an ERP; Quadra checks them, and gives you the
database, the API and the interface.

- [examples/minimal](examples/minimal/main.qd): one file with the basic concepts
- [examples/extended](examples/extended/README.md): a complete ERP
- Documentation: [quadra.run](https://quadra.run)

## Install

```bash
curl -fsSL https://quadra.run/install.sh | sh          # macOS, Linux
```

```powershell
irm https://quadra.run/install.ps1 | iex               # Windows
```

The installer puts the official binary in `~/.quadra/bin` and adds it to the PATH;
`quadra upgrade` updates it. The official binaries include **Quadra Studio**, the web
interface of `quadra dev` and `quadra serve`, which is not in this repository (see
[License](#license)).

From the sources:

```bash
cargo build --release      # target/release/quadra; QUADRA_WEB_DIST=<built studio> embeds a studio
```

## Getting started

```bash
mkdir shop && cd shop
quadra init                # name, SQLite or PostgreSQL, git
quadra dev                 # the app and the code editor on http://127.0.0.1:8080, applying every save
quadra cloud deploy        # online on Quadra Cloud (quadra cloud login first)
```

The examples:

```bash
quadra check examples/extended
quadra test examples/extended
quadra trace Invoice.due examples/extended
```

| Command | What it does |
|---|---|
| `quadra init [path] [--name n] [--db sqlite\|postgres://…] [--yes]` | a new project: starter model, tests with a `demo` fixture, git repository |
| `quadra dev [path] [--port N] [--fixture name] [--reset]` | development: the studio (the app and the code editor), every saved change checked and applied to the database at once (migrations included); problems shown in the app, the last working version keeps running |
| `quadra check [path]` | names, types, formula cycles, flows, certified metrics |
| `quadra test [path] [--filter text] [--db sqlite\|postgres://…]` | runs the `test` blocks in memory, or on a fresh database per test (reloaded and verified afterwards) |
| `quadra trace Entity.field [path]` | what a value depends on, who writes it, who reads it |
| `quadra certify [names…] [--approved-by who] [path]` | records metric fingerprints in `metrics.lock` |
| `quadra fmt [--check] [path]` | formats sources (whitespace only) |
| `quadra db init [path] [--url u]` | creates the tables and commits the seeds |
| `quadra db status [path]` | schema fingerprint, commits, last hash |
| `quadra db migrate [--dry-run] [path]` | checks the data against what the new model changes, then changes the tables that changed, in one transaction |
| `quadra db verify [path]` | checks the hash chain and that every table matches the history |
| `quadra db provisional [path]` | provisional values left by migrations, with their deadline |
| `quadra db sweep [path]` | re-evaluates the rules that read the clock (run periodically; `serve` does it hourly) |
| `quadra db fixture <name> [path]` | loads a test fixture into a database (demo data) |
| `quadra change list \| preview <name> \| apply <name> --approved-by <login> \| revert <name> --approved-by <login>` | data changes with reason, requester and approval; reverting writes a new transaction |
| `quadra serve [path] [--port 8080] [--web dir] [--dev-login] [--editors login,…] [--log-dir dir]` | the HTTP API ([reference](https://quadra.run/reference/http-api)) and the studio (or the one in `--web`); `--editors` enables the code editor (the project must be a git repository); `--log-dir` keeps every committed batch as a file in that folder (another disk) before the commit is confirmed |
| `quadra token create <login> [--read-only] [--days 90] \| revoke <name>` | API tokens for programs |
| `quadra cloud login \| deploy \| deploys \| rollback \| link \| projects \| token` | Quadra Cloud |
| `quadra upgrade` | the latest official binary in place of this one |
| `quadra lsp` | language server (used by the editor extension) |

The database is `--url`, else `[database] url` in `quadra.toml`, else
`sqlite:<project>/.quadra/data.db`. URLs: `sqlite:file.db`, `postgres://user@host:port/db`
(add `#schema=name` to use a schema).

A throwaway PostgreSQL for development, in `.pgdata` on port 54329 (the PostgreSQL
service installed on the machine is not touched):

```bash
scripts/pg.sh start     # prints the URL
scripts/pg.sh test      # every test on SQLite and PostgreSQL, same hash chain on both
scripts/pg.sh stop
```

Pending `migration` blocks run with `quadra db migrate` (preview with `--dry-run`); a fresh
`db init` counts the existing ones as applied.

## Try the example

```bash
quadra db init examples/extended --url sqlite:/tmp/demo.db
quadra db fixture italy examples/extended --url sqlite:/tmp/demo.db
quadra serve examples/extended --url sqlite:/tmp/demo.db --port 8091 --dev-login
```

With the official binary, open http://localhost:8091 and sign in as `admin`, `giulia` or
`marco`; a build from the sources serves the same API at `/v1`.

- [std/](std): the standard library (currencies, units, companies, users, numbering, time zones)
- [editors/vscode](editors/vscode/README.md): the extension for VS Code and Cursor
- [site/](site/README.md): quadra.run, the documentation and the sandbox (the engine in WebAssembly)

## License

The code of this repository is under the [Apache License 2.0](LICENSE): the language, the type
checker, the analyses and the formatter, the language server, the runtime, the storage, the
API server, the command line, the editor extension and the website.

Quadra Studio, the web interface embedded in the official binaries, has its own license: free
to use, also for work, but not to sell or to redistribute on its own. A build from these
sources has everything else.

"Quadra" and the logo are trademarks: see [TRADEMARKS.md](TRADEMARKS.md). Contributions are
welcome: see [CONTRIBUTING.md](CONTRIBUTING.md).
