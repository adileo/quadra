# Quadra for VS Code and Cursor

- Syntax highlighting (TextMate grammar) for `.qd` files
- Language server (`quadra lsp`): diagnostics while typing, go to definition, hover with the
  unified view of an entity (own members, `extend`s from other modules, traits), outline,
  semantic colouring (entities, variants, stored fields, formulas, actions, units)
- Interface files (`interfaces/**/*.qdi`: apps and saved views): highlighting, and the same
  language server: they are checked with the code (a field renamed in a `.qd` file is an
  error in the `.qdi` files that use it, at once), completion of options, entities, fields
  and `@parameters`, hover, go to definition, find references and rename across `.qd` and
  `.qdi`
- Indentation after `:` and folding by indentation

## Build and install

```bash
cargo build --release
```

```bash
cd editors/vscode && npm install && npx @vscode/vsce package --skip-license --allow-missing-repository
```

```bash
cursor --install-extension editors/vscode/quadra-0.4.0.vsix --force
```

(`code --install-extension …` for VS Code.)

## Finding the server

The extension looks for the `quadra` binary in this order:
1. the `quadra.serverPath` setting;
2. `target/release/quadra` or `target/debug/quadra` in the open workspace folder or up to
   three folders above it (so opening the repository or `examples/extended` both work);
3. `quadra` on `PATH`.

After `cargo build`, run **Developer: Reload Window** to restart the server with the new binary.
