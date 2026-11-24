# Contributing

Thank you for helping. Issues and pull requests are welcome.

## Before a pull request

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/pg.sh test        # the storage tests on PostgreSQL too (needs Docker)
```

Keep a change to one thing, with its tests. A change of the language comes with the
documentation of the website in the same pull request.

## Sign-off (DCO)

Contributions are accepted under the Apache License 2.0, the license of the project. To say
that you have the right to submit your change under it, sign off every commit with the
[Developer Certificate of Origin](https://developercertificate.org/):

```bash
git commit -s
```

which adds a line like `Signed-off-by: Your Name <you@example.com>` to the message. No other
agreement is needed.

## Quadra Studio

The web interface (`quadra dev`, `quadra serve`) is not in this repository: a build from these
sources serves the API and shows a page that points to the official binaries, which contain
it. Point `QUADRA_WEB_DIST` at a built studio to embed one.
