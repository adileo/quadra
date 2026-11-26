# quadra-site

The website of Quadra: homepage, guide, step-by-step tutorial, reference and the **Sandbox**
(the compiler and the engine in WebAssembly, the project in the browser).

```bash
npm install
npm run wasm        # the engine: crates/quadra-wasm → .vitepress/theme/sandbox/wasm (needs the wasm32 target and wasm-bindgen 0.2.129)
npm run dev         # http://localhost:5173
npm run build       # static site in .vitepress/dist
```

The demo under `/app` is the studio (`npm run app`), from the private
[quadra-studio](https://github.com/adileo/quadra-studio) checked out next to this repository
(`../studio`, or `STUDIO_SRC`); without it the site is built without the demo. CI:
`.github/workflows/site.yml`, published and deployed from `main`.

| Where | What |
|---|---|
| `index.md` | homepage |
| `guide/`, `tutorial/`, `reference/` | the documentation (Markdown; ```` ```quadra ```` blocks are highlighted with the editor's TextMate grammar, `editors/vscode/syntaxes`) |
| `projects/<name>/` | projects the sandbox opens: `welcome`, one per tutorial step (`tutorial-NN`); the full ERP comes from `examples/extended`. Each must pass `quadra check` and `quadra test` |
| `scripts/bundle-projects.mjs` | bundles them into `public/projects/*.json` (run by `dev` and `build`) |
| `.vitepress/theme/sandbox/` | the sandbox: `Sandbox.vue` (UI), `engine.ts` (WebAssembly), `store.ts` (IndexedDB: files, commits, data session), `quadra.ts` (CodeMirror language, a copy of `web/src/code/quadra.ts`) |

In a page, `<SandboxLink project="tutorial-03" file="shop.qd" />` renders an "Open in Sandbox"
button.

## How the sandbox works

- **Files and history** live in IndexedDB, one workspace per project: edits are saved as you
  type, *Commit* records a snapshot with a message, *History* restores one.
- **Checking, tests, hover, go to definition** call the engine in WebAssembly with the files
  (`check`, `test`, `ide`): the same compiler and runtime as the CLI.
- **Data** is a *session*: a fixture plus the statements typed in the console, run as a hidden
  `test` block and replayed on every change of the code — the records are always those the
  current model produces from the same history. A statement the new code refuses is flagged.
- **Download** zips the project, with the session as `tests/sandbox_data.qd`
  (`fixture sandbox`): `quadra db init && quadra db fixture sandbox && quadra serve --dev-login`
  gives the same data on a real database.
