<script setup lang="ts">
// The home's hero: the code is written a line at a time, `quadra dev` runs in the terminal, and
// Quadra Studio slides up out of it with the app those lines describe: the orders, a check that
// refuses a bad amount, a flow that confirms an order. The code (12 lines) compiles and its story
// is a passing test: keep it so. Off screen it pauses; with
// reduced motion it shows the end.
import { computed, onBeforeUnmount, onMounted, reactive, ref } from "vue";

type Row = { customer: string; amount: string; state: "Draft" | "Confirmed"; bad?: boolean };

const SOURCE = [
  "entity Customer:",
  "    name: Text",
  "",
  "entity Order:",
  "    customer: Customer",
  "    amount: Money<EUR>",
  "    state: enum(Draft, Confirmed) default Draft",
  "",
  '    check amount > 0, "The amount must be positive"',
  "",
  "    flow state:",
  "        Draft -> Confirmed by confirm",
].join("\n");
const COMMAND = "quadra dev";
const ORDERS: Row[] = [
  { customer: "Rossi Srl", amount: "1,200.00 €", state: "Draft" },
  { customer: "Bianchi SpA", amount: "850.00 €", state: "Confirmed" },
  { customer: "Verdi & Figli", amount: "2,400.00 €", state: "Draft" },
];

const code = ref("");
const typing = ref(false);
const term = reactive({ shown: false, command: "", typing: false, ready: false });
const studio = reactive({ open: false, rows: [] as Row[], selected: -1, press: false, toast: "" });

let alive = true;
let visible = true;
let wake: (() => void) | null = null;
const sleep = (ms: number) =>
  new Promise<void>((res) => {
    const t0 = performance.now();
    const tick = () => {
      if (!alive) return;
      if (!visible) { wake = () => { wake = null; tick(); }; return; }
      if (performance.now() - t0 >= ms) res();
      else setTimeout(tick, Math.min(60, ms));
    };
    tick();
  });

function reset() {
  code.value = "";
  Object.assign(term, { shown: false, command: "", typing: false, ready: false });
  Object.assign(studio, { open: false, rows: [], selected: -1, press: false, toast: "" });
}

async function run() {
  while (alive) {
    reset();
    await sleep(700);
    // the code
    typing.value = true;
    for (const ch of SOURCE) {
      if (!alive) return;
      code.value += ch;
      await sleep(ch === "\n" ? 160 : 18 + Math.random() * 26);
    }
    typing.value = false;
    await sleep(700);
    // the command
    term.shown = true;
    await sleep(350);
    term.typing = true;
    for (const ch of COMMAND) {
      term.command += ch;
      await sleep(55 + Math.random() * 40);
    }
    term.typing = false;
    await sleep(450);
    term.ready = true;
    await sleep(700);
    // Quadra Studio, out of the command
    studio.open = true;
    await sleep(650);
    for (const r of ORDERS) {
      studio.rows.push({ ...r });
      await sleep(260);
    }
    await sleep(900);
    // the check refuses a bad amount
    studio.rows.push({ customer: "Verdi & Figli", amount: "-50.00 €", state: "Draft", bad: true });
    await sleep(450);
    studio.toast = "The amount must be positive";
    await sleep(1700);
    studio.rows = studio.rows.filter((r) => !r.bad);
    studio.toast = "";
    await sleep(500);
    // the flow confirms an order
    studio.selected = 0;
    await sleep(600);
    studio.press = true;
    await sleep(220);
    studio.press = false;
    studio.rows[0].state = "Confirmed";
    await sleep(3200);
    studio.open = false;
    await sleep(700);
  }
}

// the highlighting of the editor, a line at a time (an open string is colored to its end)
const esc = (s: string) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
const TOKENS = /("(?:[^"\\]|\\.)*"?)|\b(entity|check|flow|by|default|enum)\b|\b([A-Z]\w*)\b|\b(\d[\d_.]*)\b|(->|>=|<=|[<>=*])/g;
function paint(line: string) {
  let out = "", last = 0;
  for (const m of line.matchAll(TOKENS)) {
    out += esc(line.slice(last, m.index));
    const cls = m[1] ? "s" : m[2] ? "k" : m[3] ? "t" : m[4] ? "n" : "o";
    out += `<span class="${cls}">${esc(m[0])}</span>`;
    last = m.index! + m[0].length;
  }
  return out + esc(line.slice(last));
}
const lines = computed(() => code.value.split("\n").map(paint));

const root = ref<HTMLElement | null>(null);
let io: IntersectionObserver | null = null;
onMounted(() => {
  if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
    // the end of the story, at once
    code.value = SOURCE;
    Object.assign(term, { shown: true, command: COMMAND, ready: true });
    Object.assign(studio, { open: true, rows: ORDERS.map((o, i) => ({ ...o, state: i === 0 ? "Confirmed" : o.state })) });
    return;
  }
  io = new IntersectionObserver(([e]) => { visible = e.isIntersecting; if (visible && wake) wake(); });
  if (root.value) io.observe(root.value);
  run();
});
onBeforeUnmount(() => { alive = false; io?.disconnect(); });
</script>

<template>
  <div ref="root" class="q-live" role="img" aria-label="Quadra code being written, then quadra dev, then Quadra Studio with the app it builds">
    <div class="q-live__stage">
      <div class="q-live__editor" :class="{ away: studio.open }">
        <div class="q-live__bar">
          <span class="q-live__file">orders.qd</span>
          <span class="q-live__count">{{ lines.length }} {{ lines.length === 1 ? "line" : "lines" }}</span>
        </div>
        <pre class="q-live__code"><code><template v-for="(l, i) in lines" :key="i"><span v-html="l" /><span v-if="i === lines.length - 1 && !term.shown" class="q-live__caret" :class="{ typing }" />{{ i < lines.length - 1 ? "\n" : "" }}</template></code></pre>
      </div>

      <div class="q-live__studio" :class="{ open: studio.open }" aria-hidden="true">
        <div class="q-live__appbar"><span class="q-live__url">127.0.0.1:8080</span><span class="q-live__live">Quadra Studio</span></div>
        <div class="q-live__body">
          <nav class="q-live__nav">
            <div class="q-live__brand"><svg viewBox="0 0 64 64" width="12" height="12"><rect x="4" y="4" width="26" height="26" rx="6" fill="#171717" /><rect x="34" y="4" width="26" height="26" rx="6" fill="#e5e5e5" /><rect x="4" y="34" width="26" height="26" rx="6" fill="#e5e5e5" /><rect x="34" y="34" width="26" height="26" rx="6" fill="#fde047" /></svg>Quadra</div>
            <div class="q-live__find">Find…</div>
            <div class="q-live__group">ORDERS</div>
            <div class="q-live__item">Customer</div>
            <div class="q-live__item on">Order</div>
          </nav>
          <div class="q-live__main">
            <div class="q-live__tools">
              <span class="q-live__title">Order</span>
              <span class="q-live__btn" :class="{ press: studio.press }">Confirm</span>
              <span class="q-live__btn q-live__btn--primary">+ New</span>
            </div>
            <table class="q-live__grid">
              <thead><tr><th>customer</th><th class="num">amount</th><th>state</th></tr></thead>
              <tbody>
                <tr v-for="(r, i) in studio.rows" :key="r.customer + (r.bad ? '!' : '') + i" class="q-rise" :class="{ bad: r.bad, sel: i === studio.selected }">
                  <td><span class="link">{{ r.customer }}</span></td>
                  <td class="num">{{ r.amount }}</td>
                  <td><span class="pill" :class="r.state">{{ r.state }}</span></td>
                </tr>
              </tbody>
            </table>
            <div v-if="studio.toast" class="q-live__toast q-rise"><b>{{ studio.toast }}</b> · not saved</div>
          </div>
        </div>
      </div>
    </div>

    <div class="q-live__term" :class="{ shown: term.shown }">
      <span class="p">$</span> <b>{{ term.command }}</b><span v-if="term.shown && !term.ready" class="q-live__caret" :class="{ typing: term.typing }" /><span v-if="term.ready" class="q-rise"><span class="ok">  ✓</span> Quadra Studio on <u>http://127.0.0.1:8080</u></span>
    </div>

  </div>
</template>

<style scoped>
/* the room for the editor and the terminal is kept from the start: the contents sit centred in
   it, so the page below never moves */
.q-live { position: relative; display: flex; flex-direction: column; justify-content: center; height: calc(296px + 52px); width: min(560px, 92vw); text-align: left; }

/* the stage: the editor, and the studio that slides up over it */
.q-live__stage { flex: none; position: relative; height: 296px; overflow: hidden; border-radius: var(--q-radius, 8px); background: var(--q-code-bg); }
.q-live__editor { position: absolute; inset: 0; transition: transform 0.6s cubic-bezier(0.3, 0.7, 0.2, 1), opacity 0.5s; }
.q-live__editor.away { transform: translateY(-10%); opacity: 0; }
.q-live__bar { display: flex; align-items: center; gap: 6px; padding: 12px 18px 0; }
.q-live__file { color: var(--q-c); font-family: var(--vp-font-family-mono); font-size: 11.5px; }
.q-live__count { margin-left: auto; color: var(--q-c); font-family: var(--vp-font-family-mono); font-size: 11px; }
.q-live__code { margin: 0; padding: 10px 18px; overflow: hidden; white-space: pre; color: var(--q-code-fg); font-family: var(--vp-font-family-mono); font-size: 12px; line-height: 1.65; }
.q-live__code :deep(.k) { color: var(--q-k); font-weight: 700; }
.q-live__code :deep(.t) { color: var(--q-t); }
.q-live__code :deep(.s) { color: var(--q-s); }
.q-live__code :deep(.n) { color: var(--q-n); }
.q-live__code :deep(.o) { color: var(--q-o); }
.q-live__caret { display: inline-block; width: 2px; height: 1.15em; margin-left: 1px; vertical-align: -0.2em; background: var(--q-ink); animation: q-blink 1s steps(1) infinite; }
.dark .q-live__caret { background: var(--q-hl); }
.q-live__caret.typing { animation: none; }
@keyframes q-blink { 50% { opacity: 0; } }

/* the terminal */
/* the terminal takes no room until the command runs: the editor starts centred against the text
   on the left, and rises a little as the terminal opens under it */
.q-live__term { flex: none; box-sizing: border-box; height: 0; margin-top: 0; padding: 0 18px; border-radius: var(--q-radius, 8px); background: var(--q-code-bg); color: var(--q-code-fg); font-family: var(--vp-font-family-mono); font-size: 12.5px; line-height: 20px; white-space: pre; overflow: hidden; opacity: 0; transition: height 0.45s cubic-bezier(0.3, 0.7, 0.2, 1), margin-top 0.45s cubic-bezier(0.3, 0.7, 0.2, 1), padding 0.45s cubic-bezier(0.3, 0.7, 0.2, 1), opacity 0.3s 0.1s; }
.q-live__term.shown { height: 42px; margin-top: 10px; padding: 11px 18px; opacity: 1; }
.q-live__term .p { color: var(--q-c); }
.q-live__term b { font-weight: 600; }
.q-live__term .ok { color: var(--q-ok); font-weight: 700; }
.q-live__term u { text-decoration-color: var(--q-line-strong); text-underline-offset: 3px; }

/* Quadra Studio (a light app in both themes) */
.q-live__studio { position: absolute; inset: 10px 12px 12px; display: flex; flex-direction: column; border-radius: 6px; overflow: hidden; background: #fff; color: #262626; border: 1px solid #ececec; font-family: var(--vp-font-family-base); font-size: 12px; transform: translateY(calc(100% + 16px)); transition: transform 0.65s cubic-bezier(0.3, 0.7, 0.2, 1); }
.dark .q-live__studio { border-color: transparent; }
.q-live__studio.open { transform: none; }
.q-live__appbar { display: flex; align-items: center; gap: 6px; padding: 7px 12px; background: #fafafa; border-bottom: 1px solid #f0f0f0; }
.q-live__url { font-family: var(--vp-font-family-mono); font-size: 11px; color: #8a8a8a; }
.q-live__live { margin-left: auto; font-size: 10.5px; font-weight: 500; color: #8a8a8a; }
.q-live__body { display: flex; flex: 1; min-height: 0; }
.q-live__nav { flex: none; width: 112px; padding: 10px 8px; background: #fcfcfc; border-right: 1px solid #f0f0f0; }
.q-live__brand { display: flex; align-items: center; gap: 6px; padding: 0 4px 8px; font-weight: 700; font-size: 13px; color: #171717; }
.q-live__find { margin-bottom: 8px; padding: 3px 6px; border: 1px solid #ececec; border-radius: 4px; color: #a3a3a3; font-size: 11px; }
.q-live__group { padding: 4px; font-size: 10px; color: #8a8a8a; }
.q-live__item { padding: 4px 6px; border-radius: 4px; color: #404040; }
.q-live__item.on { background: #f0f0f0; color: #171717; font-weight: 600; }
.q-live__main { position: relative; flex: 1; min-width: 0; padding: 10px 12px; overflow: hidden; }
.q-live__tools { display: flex; align-items: center; gap: 6px; margin-bottom: 8px; }
.q-live__title { margin-right: auto; font-size: 15px; font-weight: 700; color: #171717; }
.q-live__btn { padding: 2px 9px; border-radius: 4px; border: 1px solid #d4d4d4; background: #fff; font-size: 11px; font-weight: 500; color: #404040; transition: transform 0.15s, background 0.3s; }
.q-live__btn.press { transform: scale(0.92); background: #fef08a; }
.q-live__btn--primary { border-color: #171717; background: #171717; color: #fff; }
.q-live__grid { width: 100%; border-collapse: collapse; }
.q-live__grid th, .q-live__grid td { padding: 6px 8px; border-bottom: 1px solid #f0f0f0; text-align: left; white-space: nowrap; transition: background 0.4s; }
.q-live__grid th { font-size: 11px; font-weight: 600; color: #737373; }
.q-live__grid .num { text-align: right; font-variant-numeric: tabular-nums; }
.q-live__grid tr.sel td { background: #fefce8; }
.q-live__grid tr.bad td { background: #fef2f2; color: #b91c1c; }
.link { color: #171717; }
.pill { padding: 1px 6px; border-radius: 4px; font-size: 10.5px; font-weight: 500; transition: background 0.4s; }
.pill.Draft { background: #f0f0f0; color: #525252; }
.pill.Confirmed { background: #fef08a; color: #171717; }
.q-live__toast { margin-top: 8px; padding: 7px 10px; border-radius: 6px; background: #fef2f2; color: #991b1b; font-size: 11.5px; }


.q-rise { animation: q-rise 0.35s ease-out both; }
@keyframes q-rise { from { opacity: 0; transform: translateY(4px); } }

@media (prefers-reduced-motion: reduce) {
  .q-live__editor, .q-live__studio, .q-live__term { transition: none; }
}
@media (min-width: 960px) { .q-live { width: 100%; } }
@media (max-width: 959px) { .q-live { width: 100%; } }
@media (max-width: 640px) {
  .q-live { height: calc(270px + 52px); }
  .q-live__stage { height: 270px; }
  .q-live__code { font-size: 10.5px; padding: 10px 14px; }
  .q-live__nav { width: 86px; }
  .q-live__grid th, .q-live__grid td { padding: 5px 6px; }
  .q-live__term { font-size: 11px; padding: 0 14px; }
  .q-live__term.shown { padding: 11px 14px; }
}
</style>
