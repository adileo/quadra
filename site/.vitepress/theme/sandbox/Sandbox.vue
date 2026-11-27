<script setup lang="ts">
// The Quadra sandbox: a project in the browser. The files and their commits live in
// IndexedDB, the compiler and the engine run in WebAssembly, the database is a session
// (a fixture + the statements typed in the console) replayed on the current code.
import { acceptCompletion, completionStatus, type CompletionContext, type CompletionResult } from "@codemirror/autocomplete";
import { indentLess, indentMore } from "@codemirror/commands";
import { lintGutter, setDiagnostics } from "@codemirror/lint";
import { EditorSelection, EditorState, Prec } from "@codemirror/state";
import { EditorView, hoverTooltip, keymap } from "@codemirror/view";
import { basicSetup } from "codemirror";
import { strToU8, zipSync } from "fflate";
import { withBase } from "vitepress";
import { computed, nextTick, onBeforeUnmount, onMounted, ref, shallowRef, watch } from "vue";
import { quadra as q, type CheckResult, type Diagnostic, type EntityData, type Files, type SessionResult, type TestResult } from "./engine";
import { quadra as quadraLanguage } from "./quadra";
import { changes as diffFiles, newId, storage, type Workspace } from "./store";

type Template = { name: string; title: string; files?: number };

const KEYWORDS =
  "entity record trait enum extend ledger event fn action const role view report metric migration change fixture test seed import from as if else for in while return require fail emit expect expect_error expect_compile_error use clock let match when where default unique required per frozen except live flow by post to on enters becomes before key measure attr is not and or implies read edit create run deny allow hide history all rename convert backfill verify temporary load parent via through true false none self".split(
    " ",
  );
const BLANK: Files = {
  "quadra.toml": '[project]\nname = "my-app"\n',
  "main.qd": "# Your Quadra project. Start with an entity:\n\nentity Customer:\n    name: Text\n",
};

// ---- state ---------------------------------------------------------------------------------
const templates = ref<Template[]>([]);
const ws = ref<Workspace | null>(null);
const active = ref<string>("");
const tabs = ref<string[]>([]);
const check = ref<CheckResult | null>(null);
const engineError = ref<string | null>(null);
const loadingEngine = ref(true);
const panel = ref<"problems" | "tests" | "data" | "history">("problems");
const tests = ref<TestResult[] | null>(null);
const testError = ref<string | null>(null);
const testing = ref(false);
const session = ref<SessionResult | null>(null);
const running = ref(false);
const consoleText = ref("");
const consoleError = ref<string | null>(null);
const entity = ref<string | null>(null);
const showEmpty = ref(false);
const commitMessage = ref("");
const saved = ref<"saved" | "saving" | "unsaved" | "local">("saved");
const editing = ref<{ kind: "new" | "rename"; path: string; value: string } | null>(null);
const toast = ref<string | null>(null);
const showRun = ref(false);
const host = ref<HTMLDivElement | null>(null);
// the web app, beside the editor: it runs the code as last saved (⌘S)
const appOpen = ref(true);
const appFrame = ref<HTMLIFrameElement | null>(null);
const appFiles = ref<Files | null>(null);
const appSavedAt = ref<Date | null>(null);
const appWidth = ref(Number(globalThis.localStorage?.getItem("quadra.sandbox.app")) || 640);
const appUrl = withBase("/app/index.html?sandbox");
const appStale = computed(() => !!ws.value && !!appFiles.value && JSON.stringify(appFiles.value) !== JSON.stringify(ws.value.files));
const resizing = ref(false);
function dragApp(e: PointerEvent) {
  resizing.value = true;
  const start = e.clientX;
  const w0 = appWidth.value;
  const move = (ev: PointerEvent) => (appWidth.value = Math.max(320, Math.min(window.innerWidth - 420, w0 - (ev.clientX - start))));
  const up = () => {
    resizing.value = false;
    window.removeEventListener("pointermove", move);
    window.removeEventListener("pointerup", up);
    try {
      localStorage.setItem("quadra.sandbox.app", String(appWidth.value));
    } catch {
      /* not remembered */
    }
  };
  window.addEventListener("pointermove", move);
  window.addEventListener("pointerup", up);
}
/** Tell the app to reload: new code (`save`) or new data (`data`). */
function notifyApp(what: "save" | "data") {
  appFrame.value?.contentWindow?.postMessage({ type: "quadra:deploy", sha: what }, location.origin);
}
/** ⌘S: the app runs this version of the code, if it compiles. */
async function saveForApp() {
  if (!ws.value) return;
  await persist();
  await runCheck();
  if (check.value && !check.value.ok) {
    flash("Saved — the app keeps the last version that compiles: fix the errors first");
    return;
  }
  appFiles.value = JSON.parse(JSON.stringify(ws.value.files));
  appSavedAt.value = new Date();
  notifyApp("save");
  flash("Saved — the app is running your code");
}
/** Resolves once the engine is loaded and a workspace is open. */
function whenReady(): Promise<void> {
  if (ws.value && !loadingEngine.value) return Promise.resolve();
  return new Promise((resolve) => {
    const stop = watch([ws, loadingEngine], () => {
      if (ws.value && !loadingEngine.value) {
        stop();
        resolve();
      }
    });
  });
}
/** The app's API calls, answered by the engine on the saved code and the session. */
async function onAppMessage(e: MessageEvent) {
  if (e.origin !== location.origin || !appFrame.value || e.source !== appFrame.value.contentWindow) return;
  const m = e.data as { type?: string; id?: number; method?: string; path?: string; body?: unknown };
  if (m?.type !== "quadra:request") return;
  // the app loads with the page and asks at once: its first requests wait for the engine and
  // the workspace, instead of going unanswered (the app stayed on "Loading…")
  await whenReady();
  if (!ws.value) return;
  const files = appFiles.value ?? ws.value.files;
  let reply: { status: number; body: Record<string, unknown>; append?: string };
  try {
    reply = await q.app(files, ws.value.session.fixture, ws.value.session.statements, { method: m.method ?? "GET", path: m.path ?? "", body: m.body });
  } catch (err) {
    reply = { status: 500, body: { error: err instanceof Error ? err.message : String(err) } };
  }
  if (reply.append) {
    // a write of the app: a statement of the session, replayed like the console's
    ws.value.session.statements.push(reply.append);
    scheduleSave();
    if (panel.value === "data") void runSession();
  }
  (e.source as Window).postMessage({ type: "quadra:response", id: m.id, status: reply.status, body: reply.body }, location.origin);
}
const panelHeight = ref(Number(globalThis.localStorage?.getItem("quadra.sandbox.panel")) || 300);
function dragPanel(e: PointerEvent) {
  const start = e.clientY;
  const h0 = panelHeight.value;
  const move = (ev: PointerEvent) => (panelHeight.value = Math.max(120, Math.min(window.innerHeight - 220, h0 - (ev.clientY - start))));
  const up = () => {
    window.removeEventListener("pointermove", move);
    window.removeEventListener("pointerup", up);
    try {
      localStorage.setItem("quadra.sandbox.panel", String(panelHeight.value));
    } catch {
      /* not remembered */
    }
  };
  window.addEventListener("pointermove", move);
  window.addEventListener("pointerup", up);
}
const view = shallowRef<EditorView | null>(null);
const states = new Map<string, EditorState>();

const files = computed<Files>(() => ws.value?.files ?? {});
const paths = computed(() => Object.keys(files.value).sort((a, b) => depth(a) - depth(b) || a.localeCompare(b)));
const diags = computed(() => check.value?.diagnostics ?? []);
const errors = computed(() => diags.value.filter((d) => d.severity === "error").length);
const lastCommit = computed(() => ws.value?.commits[ws.value.commits.length - 1]);
const pending = computed(() => (ws.value ? diffFiles(lastCommit.value?.files ?? {}, ws.value.files) : {}));
const isPlatform = (e: EntityData) => e.module.startsWith("std.");
const entities = computed(() =>
  (session.value?.entities ?? []).filter((e) => showEmpty.value || e.count > 0).sort((a, b) => Number(isPlatform(a)) - Number(isPlatform(b))),
);
const shownEntity = computed<EntityData | undefined>(() => session.value?.entities?.find((e) => e.name === entity.value) ?? entities.value[0]);

function depth(p: string) {
  return p.split("/").length;
}
function flash(msg: string) {
  toast.value = msg;
  setTimeout(() => (toast.value = toast.value === msg ? null : toast.value), 2400);
}

// ---- tree ----------------------------------------------------------------------------------
type Node = { name: string; path: string; dir: boolean; depth: number };
const openDirs = ref<Set<string>>(new Set(["tests", "migrations"]));
const tree = computed<Node[]>(() => {
  const out: Node[] = [];
  const dirs = new Set<string>();
  for (const p of paths.value) {
    const parts = p.split("/");
    for (let i = 1; i < parts.length; i++) dirs.add(parts.slice(0, i).join("/"));
  }
  const children = (prefix: string, d: number) => {
    const sub = [...dirs].filter((x) => x.startsWith(prefix) && !x.slice(prefix.length).includes("/") && x !== prefix.replace(/\/$/, "")).sort();
    const fs = paths.value.filter((p) => p.startsWith(prefix) && !p.slice(prefix.length).includes("/")).sort();
    for (const s of sub) {
      out.push({ name: s.slice(prefix.length), path: s, dir: true, depth: d });
      if (openDirs.value.has(s)) children(`${s}/`, d + 1);
    }
    for (const f of fs) out.push({ name: f.slice(prefix.length), path: f, dir: false, depth: d });
  };
  children("", 0);
  return out;
});
function toggleDir(p: string) {
  const s = new Set(openDirs.value);
  s.has(p) ? s.delete(p) : s.add(p);
  openDirs.value = s;
}

// ---- workspaces ----------------------------------------------------------------------------
async function fetchTemplate(name: string): Promise<{ title: string; files: Files } | null> {
  if (name === "blank") return { title: "Empty project", files: { ...BLANK } };
  try {
    const r = await fetch(withBase(`/projects/${name}.json`));
    if (!r.ok) return null;
    return await r.json();
  } catch {
    return null;
  }
}

async function openWorkspace(name: string, file?: string) {
  let w = await storage.get(name);
  if (!w) {
    const t = await fetchTemplate(name);
    if (!t) {
      flash(`No project named “${name}”`);
      if (name !== "welcome") return openWorkspace("welcome");
      return;
    }
    w = { name, title: t.title, template: name, files: t.files, commits: [{ id: newId(), message: "Start from the template", at: new Date().toISOString(), files: t.files }], session: { fixture: "", statements: [] }, updated: new Date().toISOString() };
    await persist(w);
  }
  states.clear();
  ws.value = w;
  tests.value = null;
  session.value = null;
  entity.value = null;
  const first = file && w.files[file] !== undefined ? file : (Object.keys(w.files).find((p) => p.endsWith(".qd") && !p.startsWith("tests/")) ?? Object.keys(w.files)[0]);
  tabs.value = first ? [first] : [];
  for (const p of Object.keys(w.files)) {
    const dir = p.split("/").slice(0, -1).join("/");
    if (dir && p === first) openDirs.value = new Set([...openDirs.value, dir]);
  }
  active.value = first ?? "";
  history.replaceState(null, "", `${location.pathname}?project=${encodeURIComponent(name)}`);
  await nextTick();
  showFile(active.value);
  await runCheck();
  // a fresh workspace with fixtures: the data starts from the first one (`demo` if any)
  if (!w.session.fixture && w.session.statements.length === 0 && check.value?.fixtures.length) {
    const f = check.value.fixtures.find((x) => x.name === "demo") ?? check.value.fixtures[0];
    w.session.fixture = `${f.module}.${f.name}`;
    scheduleSave();
  }
  appFiles.value = JSON.parse(JSON.stringify(w.files));
  appSavedAt.value = new Date();
  notifyApp("save");
  if (panel.value === "data") void runSession();
}

let saveTimer: ReturnType<typeof setTimeout> | undefined;
async function persist(w = ws.value) {
  if (!w) return;
  w.updated = new Date().toISOString();
  saved.value = "saving";
  saved.value = (await storage.put(w)) ? "saved" : "local";
}
function scheduleSave() {
  saved.value = "unsaved";
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => void persist(), 600);
}

async function resetWorkspace() {
  if (!ws.value || !confirm("Go back to the original project? Your files and data will be replaced (your commits stay in History).")) return;
  const t = await fetchTemplate(ws.value.template);
  if (!t) return;
  ws.value.files = t.files;
  ws.value.session = { fixture: "", statements: [] };
  ws.value.commits.push({ id: newId(), message: "Reset to the template", at: new Date().toISOString(), files: t.files });
  await persist();
  await openWorkspace(ws.value.name);
}

// ---- editor --------------------------------------------------------------------------------
let checkTimer: ReturnType<typeof setTimeout> | undefined;

function completion(ctx: CompletionContext): Promise<CompletionResult | null> {
  const word = ctx.matchBefore(/[A-Za-z_][A-Za-z0-9_]*/);
  if (!word || (word.from === word.to && !ctx.explicit)) return Promise.resolve(null);
  const seen = new Set<string>();
  const options: { label: string; type: string }[] = [];
  for (const [p, text] of Object.entries(files.value)) {
    if (!p.endsWith(".qd")) continue;
    for (const m of text.matchAll(/\b(entity|record|trait|enum|fn|action|fixture|ledger)\s+([A-Za-z_]\w*)/g)) {
      if (!seen.has(m[2])) {
        seen.add(m[2]);
        options.push({ label: m[2], type: /^[A-Z]/.test(m[2]) ? "class" : "function" });
      }
    }
    for (const m of text.matchAll(/^\s+([a-z_]\w*)\s*(:|=)/gm)) {
      if (!seen.has(m[1])) {
        seen.add(m[1]);
        options.push({ label: m[1], type: "property" });
      }
    }
  }
  for (const k of KEYWORDS) if (!seen.has(k)) options.push({ label: k, type: "keyword" });
  return Promise.resolve({ from: word.from, options, validFor: /^[A-Za-z0-9_]*$/ });
}

function renderMarkdown(md: string): string {
  const esc = (t: string) => t.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  const inline = (t: string) => esc(t).replace(/`([^`]+)`/g, "<code>$1</code>").replace(/\*\*([^*]+)\*\*/g, "<b>$1</b>");
  return md
    .split(/```(?:\w+)?\n?/)
    .map((part, i) => {
      if (i % 2) return `<pre>${esc(part.trim())}</pre>`;
      let html = "";
      let list = false;
      for (const line of part.split("\n")) {
        const item = line.match(/^\s*[-*]\s+(.*)$/);
        if (item) {
          html += `${list ? "" : "<ul>"}<li>${inline(item[1])}</li>`;
          list = true;
          continue;
        }
        if (list) {
          html += "</ul>";
          list = false;
        }
        if (/^\s*---+\s*$/.test(line)) html += "<hr>";
        else if (line.trim()) html += `<p>${inline(line)}</p>`;
      }
      return list ? `${html}</ul>` : html;
    })
    .join("");
}

async function goToDefinition(v: EditorView, pos: number) {
  const r = await q.ide(files.value, active.value, pos);
  if (r.definition) openFile(r.definition.path, r.definition.from, r.definition.to);
}

function makeState(path: string, text: string): EditorState {
  return EditorState.create({
    doc: text,
    extensions: [
      Prec.highest(
        keymap.of([
          {
            key: "Tab",
            preventDefault: true,
            run: (v) => {
              if (completionStatus(v.state) === "active") return acceptCompletion(v);
              if (v.state.selection.ranges.some((r) => !r.empty)) return indentMore(v);
              v.dispatch(
                v.state.changeByRange((r) => {
                  const line = v.state.doc.lineAt(r.head);
                  const n = 4 - ((r.head - line.from) % 4);
                  return { changes: { from: r.head, insert: " ".repeat(n) }, range: EditorSelection.cursor(r.head + n) };
                }),
                { scrollIntoView: true, userEvent: "input" },
              );
              return true;
            },
            shift: indentLess,
          },
          {
            key: "Mod-s",
            preventDefault: true,
            run: () => {
              void saveForApp();
              return true;
            },
          },
          { key: "Mod-Enter", run: () => (void runTests(), true) },
          { key: "F12", run: (v) => (void goToDefinition(v, v.state.selection.main.head), true) },
        ]),
      ),
      basicSetup,
      lintGutter(),
      quadraLanguage(completion),
      EditorView.updateListener.of((u) => {
        if (u.docChanged && ws.value) {
          ws.value.files[path] = u.state.doc.toString();
          scheduleSave();
          clearTimeout(checkTimer);
          checkTimer = setTimeout(() => void runCheck(), 300);
        }
      }),
      EditorView.domEventHandlers({
        mousedown: (e, v) => {
          if (!(e.metaKey || e.ctrlKey)) return false;
          const pos = v.posAtCoords({ x: e.clientX, y: e.clientY });
          if (pos == null) return false;
          e.preventDefault();
          void goToDefinition(v, pos);
          return true;
        },
      }),
      hoverTooltip(async (_v, pos) => {
        const r = await q.ide(files.value, path, pos);
        if (!r.hover) return null;
        return {
          pos: r.hover.from,
          end: r.hover.to,
          above: true,
          create: () => {
            const dom = document.createElement("div");
            dom.className = "q-hover";
            dom.innerHTML = renderMarkdown(r.hover!.markdown);
            return { dom };
          },
        };
      }),
    ],
  });
}

function showFile(path: string) {
  const v = view.value;
  if (!v || !ws.value || ws.value.files[path] === undefined) return;
  v.setState(states.get(path) ?? makeState(path, ws.value.files[path]));
  applyDiagnostics();
}

function openFile(path: string, from?: number, to?: number) {
  if (!ws.value || ws.value.files[path] === undefined) return;
  const v = view.value;
  if (v && active.value) states.set(active.value, v.state);
  if (!tabs.value.includes(path)) tabs.value = [...tabs.value, path];
  active.value = path;
  showFile(path);
  if (v && from !== undefined) {
    const len = v.state.doc.length;
    v.dispatch({ selection: { anchor: Math.min(from, len), head: Math.min(to ?? from, len) }, effects: EditorView.scrollIntoView(Math.min(from, len), { y: "center" }) });
    v.focus();
  }
}

function closeTab(path: string) {
  const i = tabs.value.indexOf(path);
  tabs.value = tabs.value.filter((t) => t !== path);
  if (active.value === path) {
    const next = tabs.value[Math.max(0, i - 1)];
    if (next) openFile(next);
    else {
      active.value = "";
      view.value?.setState(EditorState.create({ doc: "" }));
    }
  }
}

function applyDiagnostics() {
  const v = view.value;
  if (!v) return;
  const len = v.state.doc.length;
  const ds = diags.value
    .filter((d) => d.path === active.value)
    .map((d) => ({ from: Math.min(d.from, len), to: Math.min(Math.max(d.to, d.from + 1), len), severity: d.severity, message: d.message }));
  v.dispatch(setDiagnostics(v.state, ds));
}

async function runCheck() {
  if (!ws.value) return;
  try {
    check.value = await q.check(ws.value.files);
    engineError.value = null;
  } catch (e) {
    engineError.value = `The engine failed: ${e instanceof Error ? e.message : String(e)}`;
  }
  applyDiagnostics();
  if (panel.value === "data" && check.value?.ok) scheduleSession();
}

// ---- tests ---------------------------------------------------------------------------------
async function runTests() {
  if (!ws.value) return;
  panel.value = "tests";
  testing.value = true;
  testError.value = null;
  try {
    const r = await q.test(ws.value.files);
    if (r.results) tests.value = r.results;
    else {
      tests.value = null;
      testError.value = r.error ?? `${r.diagnostics?.filter((d) => d.severity === "error").length ?? 0} error(s): fix them first (see Problems)`;
    }
  } finally {
    testing.value = false;
  }
}

// ---- data: the session ---------------------------------------------------------------------
let sessionTimer: ReturnType<typeof setTimeout> | undefined;
function scheduleSession() {
  clearTimeout(sessionTimer);
  sessionTimer = setTimeout(() => void runSession(), 400);
}

async function runSession() {
  if (!ws.value) return;
  running.value = true;
  try {
    session.value = await q.session(ws.value.files, ws.value.session.fixture, ws.value.session.statements);
  } finally {
    running.value = false;
  }
}

async function runConsole() {
  const text = consoleText.value.trim();
  if (!ws.value || !text) return;
  consoleError.value = null;
  const statements = [...ws.value.session.statements, text];
  running.value = true;
  try {
    const r = await q.session(ws.value.files, ws.value.session.fixture, statements);
    if (r.failed_at === statements.length - 1) {
      // the new statement fails: it is not kept, the data stays as it was
      consoleError.value = r.compile_error ? (r.diagnostics?.find((d) => d.path === "tests/__session__.qd")?.message ?? "Does not compile") : clean(r.error_message) || "Failed";
      return;
    }
    ws.value.session.statements = statements;
    session.value = r;
    consoleText.value = "";
    scheduleSave();
    notifyApp("data");
    // show what changed: the entity of the record just created, if any
    const m = text.match(/\b([A-Z]\w*)\s*\(/);
    if (m && r.entities?.some((e) => e.name === m[1])) entity.value = m[1];
  } finally {
    running.value = false;
  }
}

function removeStatement(i: number) {
  if (!ws.value) return;
  ws.value.session.statements.splice(i, 1);
  scheduleSave();
  void runSession();
  notifyApp("data");
}

function setFixture(f: string) {
  if (!ws.value) return;
  ws.value.session.fixture = f;
  scheduleSave();
  void runSession();
  notifyApp("data");
}

/** Messages of the session without its internal file name. */
function clean(m: string | null | undefined): string {
  return (m ?? "").replace(/tests\/__session__\.qd:\d+:\s*/g, "");
}

function cellText(c: unknown): string {
  if (c === null || c === undefined) return "";
  if (typeof c === "boolean") return c ? "✓" : "—";
  if (typeof c === "string") return c;
  if (typeof c === "object" && "d" in (c as object)) return (c as { d: string }).d;
  if (typeof c === "object" && "error" in (c as object)) return "⚠";
  return String(c);
}

// ---- history (git in the browser) and files -------------------------------------------------
async function commit() {
  if (!ws.value || !Object.keys(pending.value).length) return;
  const message = commitMessage.value.trim() || "Update";
  ws.value.commits.push({ id: newId(), message, at: new Date().toISOString(), files: { ...ws.value.files } });
  commitMessage.value = "";
  await persist();
  flash(`Committed: ${message}`);
}

async function restore(id: string) {
  const c = ws.value?.commits.find((x) => x.id === id);
  if (!ws.value || !c || !confirm(`Restore the files of “${c.message}”? Uncommitted changes are lost.`)) return;
  ws.value.files = { ...c.files };
  await persist();
  await openWorkspace(ws.value.name);
}

function startNew(dir = "") {
  editing.value = { kind: "new", path: dir, value: dir ? `${dir}/` : "" };
}
function startRename(path: string) {
  editing.value = { kind: "rename", path, value: path };
}
function submitEdit() {
  const e = editing.value;
  editing.value = null;
  if (!e || !ws.value) return;
  const target = e.value.trim().replace(/^\/+/, "");
  if (!target || target.endsWith("/")) return;
  if (!/^[\w./-]+$/.test(target) || target.includes("..")) return flash("Use letters, digits, - _ . and / in file names");
  if (ws.value.files[target] !== undefined && target !== e.path) return flash(`${target} already exists`);
  if (e.kind === "new") {
    const name = target.endsWith(".qd") || target.includes(".") ? target : `${target}.qd`;
    ws.value.files[name] = name.endsWith(".qd") ? `# ${name}\n\n` : "";
    scheduleSave();
    openFile(name);
  } else if (target !== e.path) {
    ws.value.files[target] = ws.value.files[e.path];
    delete ws.value.files[e.path];
    const st = states.get(e.path);
    if (st) states.set(target, st);
    states.delete(e.path);
    tabs.value = tabs.value.map((t) => (t === e.path ? target : t));
    if (active.value === e.path) active.value = target;
    scheduleSave();
    void runCheck();
  }
}
function removeFile(path: string) {
  if (!ws.value || !confirm(`Delete ${path}? (You can restore it from History.)`)) return;
  delete ws.value.files[path];
  states.delete(path);
  closeTab(path);
  scheduleSave();
  void runCheck();
}

/** The session as a fixture: `quadra db fixture sandbox` loads the same data locally. */
function sessionFixture(): string | null {
  const s = ws.value?.session;
  if (!s || (!s.fixture && !s.statements.length)) return null;
  const [module, name] = s.fixture ? [s.fixture.slice(0, s.fixture.lastIndexOf(".")), s.fixture.slice(s.fixture.lastIndexOf(".") + 1)] : ["", ""];
  const body = s.statements.length ? s.statements.map((st) => st.split("\n").map((l) => `    ${l}`).join("\n")).join("\n") : "    let __sandbox = 0";
  return `# The data you created in the Quadra sandbox: \`quadra db fixture sandbox\` loads it.\n${module ? `from ${module} import ${name}\n` : ""}\nfixture sandbox${name ? ` extends ${name}` : ""}:\n${body}\n`;
}

function download() {
  if (!ws.value) return;
  const root = ws.value.name;
  const out: Record<string, Uint8Array> = {};
  for (const [p, t] of Object.entries(ws.value.files)) out[`${root}/${p}`] = strToU8(t);
  const fx = sessionFixture();
  if (fx) out[`${root}/tests/sandbox_data.qd`] = strToU8(fx);
  out[`${root}/.gitignore`] = strToU8(".quadra/\n");
  const blob = new Blob([zipSync(out, { level: 6 })], { type: "application/zip" });
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  a.download = `${root}.zip`;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 2000);
  showRun.value = true;
}

// ---- lifecycle -----------------------------------------------------------------------------
watch(diags, applyDiagnostics);
watch(panel, (p) => {
  if (p === "data" && !session.value) void runSession();
});

onMounted(async () => {
  window.addEventListener("message", onAppMessage);
  view.value = new EditorView({ parent: host.value!, state: EditorState.create({ doc: "" }) });
  try {
    const r = await fetch(withBase("/projects/index.json"));
    templates.value = r.ok ? await r.json() : [];
  } catch {
    templates.value = [];
  }
  const params = new URLSearchParams(location.search);
  try {
    await import("./engine").then((m) => m.engine());
  } catch (e) {
    engineError.value = `Could not load the engine: ${e instanceof Error ? e.message : String(e)}`;
  }
  loadingEngine.value = false;
  await openWorkspace(params.get("project") ?? "welcome", params.get("file") ?? undefined);
});
onBeforeUnmount(() => {
  window.removeEventListener("message", onAppMessage);
  clearTimeout(saveTimer);
  if (ws.value && saved.value === "unsaved") void persist();
  view.value?.destroy();
});

const fixtureOptions = computed(() => (check.value?.fixtures ?? []).map((f) => `${f.module}.${f.name}`));
const statusLabel = computed(() => ({ saved: "Saved in your browser", saving: "Saving…", unsaved: "Editing…", local: "Not saved (storage blocked)" })[saved.value]);
</script>

<template>
  <div class="sb">
    <!-- top bar -->
    <header class="sb-top">
      <div class="sb-brand">Sandbox</div>
      <select class="sb-select" :value="ws?.name" @change="openWorkspace(($event.target as HTMLSelectElement).value)">
        <option v-for="t in templates" :key="t.name" :value="t.name">{{ t.title }}</option>
        <option value="blank">Empty project</option>
      </select>
      <span class="sb-status" :class="saved">{{ statusLabel }}</span>
      <span class="sb-spacer" />
      <button class="sb-btn ghost" :class="{ on: appOpen }" title="The web app of your project, running in the browser" @click="appOpen = !appOpen">▣ App</button>
      <button class="sb-btn ghost" title="Save: the app runs this version (⌘S)" @click="saveForApp">Save ⌘S</button>
      <button class="sb-btn" title="Run the tests (⌘↵)" @click="runTests">▶ Tests</button>
      <button class="sb-btn" title="Download the project as a .zip, to run it with the CLI" @click="download">⤓ Download</button>
      <button class="sb-btn ghost" title="Go back to the original project" @click="resetWorkspace">Reset</button>
    </header>

    <div v-if="engineError" class="sb-banner">{{ engineError }}</div>

    <div class="sb-body">
      <!-- files -->
      <aside class="sb-files">
        <div class="sb-files-head">
          <span>Files</span>
          <button title="New file" @click="startNew()">+</button>
        </div>
        <div v-if="editing?.kind === 'new'" class="sb-edit">
          <input v-model="editing.value" placeholder="name.qd" autofocus @keydown.enter="submitEdit" @keydown.esc="editing = null" @blur="submitEdit" />
        </div>
        <div class="sb-tree">
          <template v-for="n in tree" :key="n.path">
            <div v-if="editing?.kind === 'rename' && editing.path === n.path" class="sb-edit" :style="{ paddingLeft: `${10 + n.depth * 14}px` }">
              <input v-model="editing.value" autofocus @keydown.enter="submitEdit" @keydown.esc="editing = null" @blur="submitEdit" />
            </div>
            <div
              v-else
              class="sb-node"
              :class="{ active: !n.dir && n.path === active, dir: n.dir }"
              :style="{ paddingLeft: `${10 + n.depth * 14}px` }"
              @click="n.dir ? toggleDir(n.path) : openFile(n.path)"
              @dblclick="!n.dir && startRename(n.path)"
            >
              <span class="sb-node-icon">{{ n.dir ? (openDirs.has(n.path) ? "▾" : "▸") : n.name.endsWith(".qd") ? "◆" : "·" }}</span>
              <span class="sb-node-name">{{ n.name }}</span>
              <span v-if="!n.dir && diags.some((d) => d.path === n.path && d.severity === 'error')" class="sb-node-err">●</span>
              <span v-if="!n.dir && pending[n.path]" class="sb-node-change">{{ pending[n.path] }}</span>
              <button v-if="n.dir" class="sb-node-x" title="New file here" @click.stop="startNew(n.path)">+</button>
              <button v-else class="sb-node-x" title="Delete" @click.stop="removeFile(n.path)">×</button>
            </div>
          </template>
        </div>
        <div class="sb-files-foot">Double-click to rename · ⌘S saves · F12 / ⌘-click: definition</div>
      </aside>

      <!-- editor + panels -->
      <main class="sb-main">
        <div class="sb-tabs">
          <div v-for="t in tabs" :key="t" class="sb-tab" :class="{ active: t === active }" @click="openFile(t)">
            {{ t.split("/").pop() }}
            <button @click.stop="closeTab(t)">×</button>
          </div>
        </div>
        <div class="sb-editor">
          <div ref="host" class="sb-cm" />
          <div v-if="loadingEngine" class="sb-loading">Loading the Quadra engine…</div>
        </div>

        <div class="sb-splitter" title="Drag to resize" @pointerdown.prevent="dragPanel" />
        <section class="sb-panel" :style="{ height: `${panelHeight}px` }">
          <nav class="sb-panel-tabs">
            <button :class="{ on: panel === 'problems' }" @click="panel = 'problems'">
              Problems <span v-if="diags.length" class="sb-count" :class="{ err: errors }">{{ diags.length }}</span>
            </button>
            <button :class="{ on: panel === 'tests' }" @click="panel = 'tests'">
              Tests <span v-if="tests" class="sb-count" :class="{ err: tests.some((t) => !t.passed) }">{{ tests.filter((t) => t.passed).length }}/{{ tests.length }}</span>
            </button>
            <button :class="{ on: panel === 'data' }" @click="panel = 'data'">Data</button>
            <button :class="{ on: panel === 'history' }" @click="panel = 'history'">
              History <span v-if="Object.keys(pending).length" class="sb-count warn">{{ Object.keys(pending).length }}</span>
            </button>
          </nav>

          <!-- problems -->
          <div v-if="panel === 'problems'" class="sb-panel-body">
            <div v-if="!diags.length" class="sb-ok">✓ No problems{{ check?.ok ? ` · ${check.tests} test${check.tests === 1 ? "" : "s"} found` : "" }}</div>
            <button v-for="(d, i) in diags" :key="i" class="sb-diag" @click="openFile(d.path, d.from, d.to)">
              <span :class="d.severity">{{ d.severity === "error" ? "✕" : "▲" }}</span>
              <span class="where">{{ d.path }}:{{ d.line }}</span>
              <span class="msg">{{ d.message }}</span>
            </button>
          </div>

          <!-- tests -->
          <div v-else-if="panel === 'tests'" class="sb-panel-body">
            <div v-if="testing" class="sb-muted">Running the tests…</div>
            <div v-else-if="testError" class="sb-error">{{ testError }}</div>
            <div v-else-if="!tests" class="sb-muted">Each <code>test</code> block runs on a fresh in-memory database. <button class="sb-link" @click="runTests">Run them</button> (⌘↵ in the editor).</div>
            <div v-for="t in tests ?? []" :key="t.name" class="sb-test">
              <span :class="t.passed ? 'pass' : 'fail'">{{ t.passed ? "✓" : "✕" }}</span>
              <span class="name">{{ t.name }}</span>
              <span class="where">{{ t.location }}</span>
              <pre v-if="t.message" class="sb-test-msg">{{ t.message }}</pre>
            </div>
          </div>

          <!-- data -->
          <div v-else-if="panel === 'data'" class="sb-panel-body sb-data">
            <div class="sb-data-side">
              <label class="sb-label">Start from</label>
              <select class="sb-select full" :value="ws?.session.fixture ?? ''" @change="setFixture(($event.target as HTMLSelectElement).value)">
                <option value="">empty database</option>
                <option v-for="f in fixtureOptions" :key="f" :value="f">fixture {{ f.split(".").pop() }}</option>
              </select>
              <label class="sb-label">Console <span class="sb-muted">⌘↵ runs</span></label>
              <textarea
                v-model="consoleText"
                class="sb-console"
                spellcheck="false"
                placeholder="Customer(name=&quot;Initech&quot;, credit_limit=2000.00 EUR)"
                @keydown.meta.enter.prevent="runConsole"
                @keydown.ctrl.enter.prevent="runConsole"
              />
              <div v-if="consoleError" class="sb-error small">{{ consoleError }}</div>
              <label class="sb-label">Session · replayed on every change of the code</label>
              <ol class="sb-statements">
                <li v-for="(s, i) in ws?.session.statements ?? []" :key="i" :class="{ failed: session?.failed_at === i }">
                  <code>{{ s }}</code>
                  <button title="Remove" @click="removeStatement(i)">×</button>
                </li>
                <li v-if="!ws?.session.statements.length" class="sb-muted">Nothing yet: create records with the console.</li>
              </ol>
              <div v-if="session?.failed_at != null && session.failed_at < (ws?.session.statements.length ?? 0)" class="sb-error small">
                With the current code, statement {{ session.failed_at + 1 }} fails: {{ clean(session.error_message ?? session.diagnostics?.[0]?.message) }}
              </div>
              <div v-else-if="session?.error_message" class="sb-error small">{{ clean(session.error_message) }}</div>
            </div>
            <div class="sb-data-main">
              <div class="sb-entities">
                <button v-for="e in entities" :key="e.name" :class="{ on: shownEntity?.name === e.name, platform: isPlatform(e) }" :title="e.module" @click="entity = e.name">
                  {{ e.name }} <span>{{ e.count }}</span>
                </button>
                <label class="sb-empty-toggle"><input v-model="showEmpty" type="checkbox" /> empty</label>
                <span v-if="running" class="sb-muted">running…</span>
              </div>
              <div v-if="shownEntity" class="sb-table-wrap">
                <table class="sb-table">
                  <thead>
                    <tr>
                      <th>#</th>
                      <th v-for="f in shownEntity.fields" :key="f">{{ f }}</th>
                    </tr>
                  </thead>
                  <tbody>
                    <tr v-for="r in shownEntity.rows" :key="r.id">
                      <td class="id">{{ r.id }}</td>
                      <td v-for="(c, i) in r.cells" :key="i" :class="{ ref: c && typeof c === 'object' && 'ref' in c, err: c && typeof c === 'object' && 'error' in c }" :title="c && typeof c === 'object' && 'error' in c ? (c as any).error : ''" @click="c && typeof c === 'object' && 'ent' in c && (entity = (c as any).ent)">
                        {{ cellText(c) }}
                      </td>
                    </tr>
                  </tbody>
                </table>
              </div>
              <div v-else class="sb-muted pad">{{ check && !check.ok ? "Fix the errors to see the data." : "No records yet." }}</div>
            </div>
          </div>

          <!-- history -->
          <div v-else class="sb-panel-body sb-history">
            <div class="sb-commit">
              <div class="sb-label">Changes since the last commit</div>
              <div v-if="!Object.keys(pending).length" class="sb-muted">Nothing to commit.</div>
              <div v-for="(k, p) in pending" :key="p" class="sb-change"><b :class="k">{{ k }}</b> {{ p }}</div>
              <div class="sb-commit-row">
                <input v-model="commitMessage" placeholder="What changed, and why" @keydown.enter="commit" />
                <button class="sb-btn" :disabled="!Object.keys(pending).length" @click="commit">Commit</button>
              </div>
            </div>
            <div class="sb-commits">
              <div class="sb-label">History · kept in this browser</div>
              <div v-for="c in [...(ws?.commits ?? [])].reverse()" :key="c.id" class="sb-commit-item">
                <span class="sha">{{ c.id }}</span>
                <span class="msg">{{ c.message }}</span>
                <span class="at">{{ new Date(c.at).toLocaleString() }}</span>
                <button class="sb-link" @click="restore(c.id)">restore</button>
              </div>
            </div>
          </div>
        </section>
      </main>

      <template v-if="appOpen">
        <div class="sb-vsplit" title="Drag to resize" @pointerdown.prevent="dragApp" />
        <section class="sb-app" :style="{ width: `${appWidth}px` }">
          <div class="sb-app-bar">
            <span class="sb-app-title">App</span>
            <span v-if="appStale" class="sb-app-stale">unsaved changes · ⌘S to update</span>
            <span v-else class="sb-muted">running the saved code{{ appSavedAt ? ` · ${appSavedAt.toLocaleTimeString()}` : "" }}</span>
            <span class="sb-spacer" />
            <button class="sb-link" title="Reload the app" @click="notifyApp('save')">↻</button>
          </div>
          <iframe ref="appFrame" class="sb-app-frame" :class="{ resizing }" :src="appUrl" title="The web app of the project" />
        </section>
      </template>
    </div>

    <div v-if="toast" class="sb-toast">{{ toast }}</div>

    <div v-if="showRun" class="sb-modal" @click.self="showRun = false">
      <div class="sb-modal-card">
        <h3>Run it on your machine</h3>
        <p>Unzip <b>{{ ws?.name }}.zip</b>, then with the <a :href="withBase('/guide/installation')">Quadra CLI</a>:</p>
        <pre>cd {{ ws?.name }}
quadra check
quadra test
quadra db init
<template v-if="ws?.session.fixture || ws?.session.statements.length">quadra db fixture sandbox      # the data of your session
</template>quadra serve --dev-login</pre>
        <p class="sb-muted">Then open the web app on <code>http://localhost:8080</code>.</p>
        <button class="sb-btn" @click="showRun = false">Got it</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
/* dark controls and scrollbars, whatever the site's theme */
.sb, .sb :deep(*) { color-scheme: dark; scrollbar-color: #3a3b40 transparent; }
.sb :deep(::-webkit-scrollbar) { width: 10px; height: 10px; }
.sb :deep(::-webkit-scrollbar-track) { background: transparent; }
.sb :deep(::-webkit-scrollbar-thumb) { background: #3a3b40; border-radius: 5px; border: 2px solid transparent; background-clip: padding-box; }
.sb :deep(::-webkit-scrollbar-thumb:hover) { background-color: #4a4b50; }
.sb :deep(::-webkit-scrollbar-corner) { background: transparent; }
.sb { --bg: #141517; --panel: #1a1b1e; --line: rgba(255, 255, 255, 0.07); --text: #e5e5e5; --muted: #8a8a8a; --accent: #fde047;
  position: relative; display: flex; flex-direction: column; height: calc(100vh - var(--vp-nav-height)); background: var(--bg); color: var(--text); font-size: 13px; }
.sb-top { display: flex; align-items: center; gap: 10px; padding: 8px 14px; border-bottom: 1px solid var(--line); background: var(--panel); }
.sb-brand { font-weight: 700; letter-spacing: 0.02em; color: #fff; }
.sb-select { background: #1e1f22; color: var(--text); border: 1px solid var(--line); border-radius: 8px; padding: 5px 8px; font-size: 12.5px; }
.sb-select.full { width: 100%; }
.sb-status { font-size: 11.5px; color: var(--muted); }
.sb-status.local { color: #fbbf24; }
.sb-spacer { flex: 1; }
.sb-btn { background: #fde047; color: #141517; border-radius: 8px; padding: 5px 12px; font-size: 12.5px; font-weight: 600; }
.sb-btn:disabled { opacity: 0.4; }
.sb-btn.ghost { background: transparent; color: var(--muted); border: 1px solid var(--line); }
.sb-btn:hover:not(:disabled) { filter: brightness(1.12); }
.sb-banner { padding: 8px 14px; background: rgba(244, 63, 94, 0.12); color: #fda4af; border-bottom: 1px solid rgba(244, 63, 94, 0.3); }
.sb-body { flex: 1; display: flex; min-height: 0; }

.sb-files { width: 220px; flex-shrink: 0; display: flex; flex-direction: column; border-right: 1px solid var(--line); background: var(--panel); }
.sb-files-head { display: flex; justify-content: space-between; align-items: center; padding: 10px 12px 6px; font-size: 11px; font-weight: 700; letter-spacing: 0.14em; text-transform: uppercase; color: var(--muted); }
.sb-files-head button { color: var(--muted); font-size: 16px; line-height: 1; }
.sb-files-head button:hover { color: #fff; }
.sb-tree { flex: 1; overflow: auto; padding-bottom: 8px; font-family: var(--vp-font-family-mono); font-size: 12.5px; }
.sb-node { position: relative; display: flex; align-items: center; gap: 6px; height: 24px; padding-right: 8px; cursor: pointer; color: #c4c4c4; }
.sb-node:hover { background: rgba(255, 255, 255, 0.04); }
.sb-node.active { background: rgba(253, 224, 71, 0.22); color: #fff; }
.sb-node-icon { width: 12px; color: #a3a3a3; font-size: 10px; text-align: center; }
.sb-node.dir .sb-node-icon { color: var(--muted); }
.sb-node-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sb-node-err { color: #fb7185; font-size: 9px; }
.sb-node-change { margin-left: auto; color: #fbbf24; font-size: 10.5px; }
.sb-node-x { display: none; margin-left: auto; color: var(--muted); }
.sb-node:hover .sb-node-x { display: block; }
.sb-node:hover .sb-node-change { display: none; }
.sb-node-x:hover { color: #fff; }
.sb-edit { padding: 2px 8px 2px 10px; }
.sb-edit input { width: 100%; background: #141517; border: 1px solid #fde047; border-radius: 5px; padding: 2px 6px; color: #fff; font-family: var(--vp-font-family-mono); font-size: 12px; outline: none; }
.sb-files-foot { padding: 8px 12px; border-top: 1px solid var(--line); font-size: 10.5px; color: #5c5c5c; line-height: 1.5; }

.sb-main { flex: 1; min-width: 0; display: flex; flex-direction: column; }
.sb-tabs { display: flex; gap: 1px; height: 36px; align-items: flex-end; padding: 0 8px; background: var(--panel); border-bottom: 1px solid var(--line); overflow-x: auto; }
.sb-tab { display: flex; align-items: center; gap: 8px; height: 32px; padding: 0 12px; border-radius: 8px 8px 0 0; font-family: var(--vp-font-family-mono); font-size: 12px; color: var(--muted); cursor: pointer; white-space: nowrap; }
.sb-tab.active { background: #17181b; color: #fff; }
.sb-tab button { color: var(--muted); }
.sb-tab button:hover { color: #fff; }
.sb-editor { position: relative; flex: 1; min-height: 0; background: #17181b; }
.sb-cm { height: 100%; }
.sb-cm :deep(.cm-editor) { height: 100%; }
.sb-loading { position: absolute; inset: 0; display: flex; align-items: center; justify-content: center; color: var(--muted); background: #17181b; }

.sb-splitter { height: 5px; margin-top: -2px; cursor: row-resize; position: relative; z-index: 2; }
.sb-splitter:hover { background: rgba(253, 224, 71, 0.5); }
.sb-panel { flex-shrink: 0; display: flex; flex-direction: column; border-top: 1px solid var(--line); background: var(--panel); }
.sb-panel-tabs { display: flex; gap: 4px; padding: 6px 10px 0; border-bottom: 1px solid var(--line); }
.sb-panel-tabs button { padding: 6px 10px; font-size: 12px; color: var(--muted); border-bottom: 2px solid transparent; }
.sb-panel-tabs button.on { color: #fff; border-bottom-color: var(--accent); }
.sb-count { margin-left: 4px; padding: 0 6px; border-radius: 999px; background: rgba(134, 239, 172, 0.15); color: #86efac; font-size: 10.5px; }
.sb-count.err { background: rgba(244, 63, 94, 0.18); color: #fda4af; }
.sb-count.warn { background: rgba(251, 191, 36, 0.15); color: #fcd34d; }
.sb-panel-body { flex: 1; overflow: auto; padding: 8px 12px; font-family: var(--vp-font-family-mono); font-size: 12px; }
.sb-ok { color: #86efac; }
.sb-muted { color: var(--muted); }
.sb-muted.pad { padding: 16px; }
.sb-error { color: #fda4af; white-space: pre-wrap; }
.sb-error.small { margin-top: 6px; font-size: 11.5px; }
.sb-link { color: var(--accent); text-decoration: underline; }
.sb-diag { display: flex; gap: 10px; width: 100%; padding: 3px 6px; text-align: left; border-radius: 4px; }
.sb-diag:hover { background: rgba(255, 255, 255, 0.04); }
.sb-diag .error { color: #fb7185; }
.sb-diag .warning { color: #fbbf24; }
.sb-diag .where { color: var(--muted); flex-shrink: 0; }
.sb-test { padding: 3px 6px; }
.sb-test .pass { color: #86efac; }
.sb-test .fail { color: #fb7185; }
.sb-test .name { margin: 0 8px; color: #fff; }
.sb-test .where { color: var(--muted); }
.sb-test-msg { margin: 4px 0 6px 18px; color: #fda4af; white-space: pre-wrap; }

.sb-data { display: flex; gap: 12px; padding: 0; }
.sb-data-side { width: 340px; flex-shrink: 0; padding: 10px 12px; border-right: 1px solid var(--line); overflow: auto; }
.sb-label { display: block; margin: 10px 0 4px; font-family: var(--vp-font-family-base); font-size: 10.5px; font-weight: 700; letter-spacing: 0.1em; text-transform: uppercase; color: var(--muted); }
.sb-label:first-child { margin-top: 0; }
.sb-console { width: 100%; min-height: 64px; padding: 8px; border-radius: 8px; border: 1px solid var(--line); background: #141517; color: #fff; font-family: var(--vp-font-family-mono); font-size: 12px; resize: vertical; outline: none; }
.sb-console:focus { border-color: #fde047; }
.sb-statements { margin: 0; padding-left: 18px; color: var(--muted); }
.sb-statements li { position: relative; padding: 2px 18px 2px 0; }
.sb-statements li code { color: #d4d4d4; white-space: pre-wrap; word-break: break-word; }
.sb-statements li.failed code { color: #fda4af; text-decoration: underline wavy #fb7185; }
.sb-statements li button { position: absolute; right: 0; top: 2px; color: var(--muted); }
.sb-statements li button:hover { color: #fff; }
.sb-data-main { flex: 1; min-width: 0; display: flex; flex-direction: column; padding: 8px 12px 8px 0; }
.sb-entities { display: flex; flex-wrap: wrap; gap: 4px; align-items: center; margin-bottom: 8px; }
.sb-entities button { padding: 3px 8px; border-radius: 6px; color: #c4c4c4; background: rgba(255, 255, 255, 0.03); }
.sb-entities button span { color: var(--muted); margin-left: 4px; }
.sb-entities button.on { background: rgba(253, 224, 71, 0.3); color: #fff; }
.sb-entities button.platform { opacity: 0.6; }
.sb-empty-toggle { margin-left: 6px; color: var(--muted); font-size: 11px; display: inline-flex; gap: 4px; align-items: center; }
.sb-table-wrap { flex: 1; overflow: auto; border: 1px solid var(--line); border-radius: 8px; }
.sb-table { border-collapse: collapse; width: max-content; min-width: 100%; margin: 0; display: table; }
.sb-table th, .sb-table td { padding: 4px 10px; border: none; border-bottom: 1px solid var(--line); text-align: left; white-space: nowrap; max-width: 260px; overflow: hidden; text-overflow: ellipsis; font-size: 12px; }
.sb-table th { position: sticky; top: 0; background: #1e1f22; color: var(--muted); font-weight: 600; }
.sb-table tr { background: transparent !important; }
.sb-table td.id { color: var(--muted); }
.sb-table td.ref { color: #ededed; cursor: pointer; }
.sb-table td.err { color: #fb7185; }

.sb-history { display: flex; gap: 24px; }
.sb-commit { width: 380px; flex-shrink: 0; }
.sb-change b { display: inline-block; width: 16px; }
.sb-change .A { color: #86efac; }
.sb-change .M { color: #fcd34d; }
.sb-change .D { color: #fb7185; }
.sb-commit-row { display: flex; gap: 8px; margin-top: 10px; }
.sb-commit-row input { flex: 1; background: #141517; border: 1px solid var(--line); border-radius: 8px; padding: 5px 8px; color: #fff; outline: none; }
.sb-commits { flex: 1; min-width: 0; }
.sb-commit-item { display: flex; gap: 10px; align-items: baseline; padding: 3px 0; }
.sb-commit-item .sha { color: var(--accent); }
.sb-commit-item .msg { color: #fff; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sb-commit-item .at { margin-left: auto; color: var(--muted); font-size: 11px; white-space: nowrap; }

.sb-btn.ghost.on { color: #fff; border-color: rgba(253, 224, 71, 0.6); background: rgba(253, 224, 71, 0.18); }
.sb-vsplit { width: 7px; margin: 0 -3px; cursor: col-resize; position: relative; z-index: 2; }
.sb-vsplit:hover { background: rgba(253, 224, 71, 0.5); }
.sb-app { flex-shrink: 0; display: flex; flex-direction: column; border-left: 1px solid var(--line); background: #fff; }
.sb-app-bar { display: flex; align-items: center; gap: 10px; padding: 6px 12px; background: var(--panel); border-bottom: 1px solid var(--line); font-size: 11.5px; }
.sb-app-title { font-weight: 700; color: #fff; letter-spacing: 0.02em; }
.sb-app-stale { color: #fbbf24; }
.sb-app-frame { flex: 1; width: 100%; border: 0; background: #fff; }
.sb-app-frame.resizing { pointer-events: none; }
.sb-toast { position: absolute; bottom: 20px; left: 50%; transform: translateX(-50%); padding: 8px 14px; border-radius: 8px; background: #232428; border: 1px solid rgba(253, 224, 71, 0.4); color: #fff; }
.sb-modal { position: absolute; inset: 0; display: flex; align-items: center; justify-content: center; background: rgba(5, 7, 12, 0.7); z-index: 10; }
.sb-modal-card { width: min(520px, 92vw); padding: 22px 24px; border-radius: 8px; background: #1c1d20; border: 1px solid rgba(253, 224, 71, 0.3); }
.sb-modal-card h3 { margin: 0 0 8px; font-size: 18px; color: #fff; }
.sb-modal-card pre { margin: 10px 0; padding: 12px; border-radius: 8px; background: #141517; color: #d4d4d4; font-family: var(--vp-font-family-mono); font-size: 12.5px; }
.sb-modal-card a { color: var(--accent); }
.sb :deep(.q-hover) { padding: 8px 12px; font-size: 12.5px; line-height: 1.55; max-width: 520px; }
.sb :deep(.q-hover pre) { margin: 0 0 6px; padding: 6px 8px; background: #17181b; border-radius: 6px; white-space: pre-wrap; font-size: 12px; }
.sb :deep(.q-hover p) { margin: 2px 0; }
.sb :deep(.q-hover ul) { margin: 4px 0; padding-left: 16px; }
.sb :deep(.q-hover li) { margin: 1px 0; }
.sb :deep(.q-hover hr) { border: none; border-top: 1px solid rgba(255, 255, 255, 0.08); margin: 6px 0; }
.sb :deep(.q-hover code) { background: #17181b; padding: 0 4px; border-radius: 4px; color: #9fe0b0; font-size: 12px; }

@media (max-width: 900px) {
  .sb-files { display: none; }
  .sb-data-side { width: 240px; }
}
</style>
