# Installation

Quadra is a single command-line program, `quadra`, written in Rust. It contains the compiler,
the test runner, the database tools, the HTTP server and the language server for your editor.
The standard library (currencies, units, companies, users, numbering) is built into the binary.

::: tip Just want to look around?
You don't need to install anything to try the language: the [Sandbox](/sandbox) runs the
compiler and the tests in your browser.
:::

## Install

On macOS and Linux:

```bash
curl -fsSL https://quadra.run/install.sh | sh
```

On Windows (PowerShell):

```powershell
irm https://quadra.run/install.ps1 | iex
```

The installer picks the binary for your machine (macOS on Apple chips and Intel, Linux on x64
and ARM, Windows x64), checks its SHA-256, puts it in `~/.quadra/bin` and adds that folder to
your `PATH`. Open a new terminal and check:

```bash
quadra --version
```

To update, run `quadra upgrade`. A specific version: `QUADRA_VERSION=0.5.0` before the command
(`$env:QUADRA_VERSION = "0.5.0"` on Windows). To leave your shell's configuration alone, set
`QUADRA_NO_MODIFY_PATH=1` and add `~/.quadra/bin` to the `PATH` yourself.

::: details What the installer changes
It writes `~/.quadra/bin/quadra` and one line in the configuration of your shell:
`~/.zshrc` (zsh), `~/.bashrc` and `~/.bash_profile` (bash), `~/.config/fish/conf.d/quadra.fish`
(fish) or `~/.profile`. On Windows it adds the folder to your user `PATH`. To uninstall, remove
`~/.quadra` and that line.
:::

## Build from source

Quadra is open source ([github.com/adileo/quadra](https://github.com/adileo/quadra), Apache
License 2.0). You need a recent stable Rust toolchain ([rustup.rs](https://rustup.rs)):

```bash
git clone https://github.com/adileo/quadra.git
cd quadra
cargo build --release -p quadra-cli      # target/release/quadra
```

A build from the sources has everything except **Quadra Studio**, the web interface of
`quadra dev` and `quadra serve` (the spreadsheet-like app and the code editor): it is in the
official binaries, under its own license (free to use, not to resell). Without it, the API
answers as usual at `/v1`.

Run `quadra` without arguments to see every command:

```text
usage:
  quadra init [path] [--name n] [--db sqlite|postgres://…] [--yes]   a new project
  quadra dev [path] [--port 8080] [--fixture name] [--reset]   app + code editor, applying every saved change
  quadra check [path]                  check a project (default: current directory)
  quadra trace <Entity.field> [path]   what a value depends on and who writes it
  quadra certify [name…] [path]        record metric fingerprints in metrics.lock
  quadra fmt [--check] [path]          format sources
  quadra test [path] [--filter text] [--db sqlite|postgres://…]   run the tests
  quadra db init|status|migrate [--dry-run]|verify|provisional|sweep|fixture <name> [path] [--url URL]
  quadra change list | preview <name> | apply <name> [--approved-by login] | revert <name> [--approved-by login]
  quadra serve [path] [--url URL] [--port 8080] [--web dir] [--dev-login] [--cache-mb 256] [--prewarm-days N] [--sweep-every secs] [--editors login,…]
  quadra token create <login> [--name n] [--read-only] [--days 90] | revoke <name> [path] [--url URL]
  quadra parse <file>                  print the syntax tree of a file
  quadra lsp                           start the language server
```

## The studio

The studio (the app your users work in, and the code editor) is built into the official
binary: `quadra dev` and `quadra serve` serve it on the same origin as the API, with nothing
else to install. `--web dir` serves another build of it instead.

## Editor support: VS Code and Cursor

The extension in `editors/vscode` gives you:

- syntax highlighting for `.qd` files;
- the language server (`quadra lsp`): errors while you type, go to definition, hover with
  types and units (and the unified view of an entity spread over several `extend`s), outline,
  semantic colouring of entities, stored fields, formulas, actions and units, and hints on
  how queries, row rules and formulas run on the database ([`quadra lsp`](/reference/cli#quadra-lsp));
- indentation after `:` and folding by indentation.

Build and install it from the sources ([github.com/adileo/quadra](https://github.com/adileo/quadra)):

```bash
cd editors/vscode
npm install
npx @vscode/vsce package --skip-license --allow-missing-repository
```

```bash
code --install-extension quadra-*.vsix --force       # VS Code
cursor --install-extension quadra-*.vsix --force     # Cursor
```

The extension looks for the `quadra` binary in this order:

1. the `quadra.serverPath` setting;
2. `target/release/quadra` or `target/debug/quadra` in the open folder or up to three folders
   above it;
3. `quadra` on your `PATH`.

After rebuilding `quadra`, run **Developer: Reload Window** to restart the language server.

## Project layout

A Quadra project is a folder with a `quadra.toml` file. Every `.qd` file inside it is a
**module**, named after its path: `catalog.qd` is `catalog`, `core/base.qd` is `core.base`.

```text
shop/
├── quadra.toml             the project file
├── customers.qd            modules: entities, formulas, rules, flows…
├── catalog.qd
├── orders.qd
├── security.qd             roles and permissions
├── tests/
│   ├── fixtures.qd         shared test data (`fixture` blocks)
│   └── orders.test.qd      `test` blocks
├── migrations/
│   ├── 2026-12-01_sku.qd   `migration` blocks that go with a schema change
│   └── origins.csv         data a migration loads
└── .quadra/data.db         the development database (created by `quadra db init`)
```

The smallest `quadra.toml`:

```toml
[project]
name = "shop"
```

Add the project's time zone (an IANA name) if it is not UTC: `today()`, the hours and the
dates of instants are read there ([Dates, times and time zones](/reference/types#dates-times-and-time-zones)):

```toml
[project]
name = "shop"
timezone = "Europe/Rome"
```

By default the database is SQLite in `.quadra/data.db`. To use another one, pass `--url` to
the `db`, `serve` and `token` commands, or set it in `quadra.toml`:

```toml
[database]
url = "postgres://quadra@localhost:5432/shop"
```

URLs look like `sqlite:path/to/file.db` or `postgres://user@host:port/db` (add
`#schema=name` to use a PostgreSQL schema). A relative SQLite path in `quadra.toml` is
relative to the project folder.

### Modules and imports

A module sees the standard library (`EUR`, `kg`, `Currency`, `Uom`, `Company`, `User`…) and
what it imports:

```quadra
from customers import Customer, Address
from catalog import Product, Category
```

Imports cannot form a cycle. When a lower module needs a field that belongs to a higher one
(customers do not know about orders, but a customer has orders), the higher module adds it
with `extend`:

```quadra
extend Customer:
    orders: [Order]
```

Test files (`*.test.qd` and the other files in `tests/`) and `migrations/` see every module of
the project without imports; tests import only the fixtures they use.

## Next

Head to the [Quick start](./quick-start) to create your first project.
