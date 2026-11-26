<script setup lang="ts">
// The home's hero: the code is written a line at a time, and the app follows each line. An entity
// appears in the menu, a field becomes a column, a check refuses a bad value, a flow adds its
// button. The final code (12 lines) compiles and its story is a passing test: keep it so, and
// keep the caption's count true. Off screen it pauses; with reduced motion it shows the end.
import { computed, onBeforeUnmount, onMounted, reactive, ref } from "vue";

type Row = { customer: string; amount: string; state: "Draft" | "Confirmed"; bad?: boolean };

const code = ref("");
const typing = ref(false);
const app = reactive({
  entities: [] as string[],
  active: "" as string,
  fresh: "" as string, // the entity or column just added, highlighted for a moment
  cols: [] as string[],
  button: false,
  press: false,
  selected: -1,
  toast: "",
  rows: [] as Row[],
});
const customers = ["Rossi Srl", "Bianchi SpA", "Verdi & Figli"];
const orders: Row[] = [
  { customer: "Rossi Srl", amount: "1,200.00 €", state: "Draft" },
  { customer: "Bianchi SpA", amount: "850.00 €", state: "Confirmed" },
  { customer: "Verdi & Figli", amount: "2,400.00 €", state: "Draft" },
];

// what is typed, and what the app does once it is
const steps: { text: string; then: () => Promise<void> | void }[] = [
  { text: "entity Customer:", then: () => addEntity("Customer") },
  { text: "\n    name: Text", then: () => addCol("name") },
  { text: "\n\nentity Order:", then: () => addEntity("Order") },
  { text: "\n    customer: Customer", then: () => addCol("customer") },
  { text: "\n    amount: Money<EUR>", then: () => addCol("amount") },
  { text: "\n    state: enum(Draft, Confirmed) default Draft", then: () => addCol("state") },
  { text: '\n\n    check amount > 0, "The amount must be positive"', then: refuse },
  { text: "\n\n    flow state:\n        Draft -> Confirmed by confirm", then: confirm },
];

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
  Object.assign(app, { entities: [], active: "", fresh: "", cols: [], button: false, press: false, selected: -1, toast: "", rows: [] });
}
function flash(name: string) {
  app.fresh = name;
  setTimeout(() => { if (app.fresh === name) app.fresh = ""; }, 1100);
}
function addEntity(name: string) {
  app.entities.push(name);
  app.active = name;
  app.cols = [];
  app.rows = [];
  flash(name);
}
function addCol(col: string) {
  app.cols.push(col);
  if (app.active === "Customer") app.rows = customers.map((c) => ({ customer: c, amount: "", state: "Draft" }));
  else if (app.rows.length === 0) app.rows = orders.map((o) => ({ ...o }));
  flash(col);
}
async function refuse() {
  app.rows.push({ customer: "Verdi & Figli", amount: "-50.00 €", state: "Draft", bad: true });
  await sleep(500);
  app.toast = "The amount must be positive";
  await sleep(1700);
  app.rows = app.rows.filter((r) => !r.bad);
  app.toast = "";
}
async function confirm() {
  app.button = true;
  flash("confirm");
  await sleep(700);
  app.selected = 0;
  await sleep(600);
  app.press = true;
  await sleep(250);
  app.press = false;
  app.rows[0].state = "Confirmed";
}

async function run() {
  while (alive) {
    reset();
    await sleep(600);
    for (const step of steps) {
      typing.value = true;
      for (const ch of step.text) {
        if (!alive) return;
        code.value += ch;
        await sleep(ch === "\n" ? 140 : 26 + Math.random() * 30);
      }
      typing.value = false;
      await sleep(250);
      await step.then();
      await sleep(900);
    }
    await sleep(4200);
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
    code.value = steps.map((s) => s.text).join("");
    Object.assign(app, { entities: ["Customer", "Order"], active: "Order", cols: ["customer", "amount", "state"], button: true, rows: orders.map((o, i) => ({ ...o, state: i === 0 ? "Confirmed" : o.state })) });
    return;
  }
  io = new IntersectionObserver(([e]) => { visible = e.isIntersecting; if (visible && wake) wake(); });
  if (root.value) io.observe(root.value);
  run();
});
onBeforeUnmount(() => { alive = false; io?.disconnect(); });
</script>

<template>
  <div ref="root" class="q-live" role="img" aria-label="Quadra code being written, and the app it builds changing with every line">
    <div class="q-live__editor">
      <div class="q-live__bar">
        <span class="dot" /><span class="dot" /><span class="dot" />
        <span class="q-live__file">orders.qd</span>
        <span class="q-live__count">{{ lines.length }} {{ lines.length === 1 ? "line" : "lines" }}</span>
      </div>
      <pre class="q-live__code"><code><template v-for="(l, i) in lines" :key="i"><span v-html="l" /><span v-if="i === lines.length - 1" class="q-live__caret" :class="{ typing }" />{{ i < lines.length - 1 ? "\n" : "" }}</template></code></pre>
    </div>

    <div class="q-live__app">
      <div class="q-live__appbar"><span class="dot" /><span class="dot" /><span class="dot" /><span class="q-live__url">your app</span><span class="q-live__live">● live</span></div>
      <div class="q-live__body">
        <nav class="q-live__nav">
          <div class="q-live__brand">Quadra</div>
          <div v-for="e in app.entities" :key="e" class="q-live__item q-pop" :class="{ on: e === app.active, fresh: e === app.fresh }">{{ e }}</div>
          <div v-if="!app.entities.length" class="q-live__empty">No entities yet</div>
        </nav>
        <div class="q-live__main">
          <div class="q-live__tools">
            <span class="q-live__title">{{ app.active || " " }}</span>
            <span v-if="app.button" class="q-live__btn q-pop" :class="{ fresh: app.fresh === 'confirm', press: app.press }">Confirm</span>
            <span v-if="app.active" class="q-live__btn q-live__btn--primary">+ New</span>
          </div>
          <table v-if="app.cols.length" class="q-live__grid">
            <thead>
              <tr><th v-for="c in app.cols" :key="c" :class="{ fresh: c === app.fresh }">{{ c }}</th></tr>
            </thead>
            <tbody>
              <tr v-for="(r, i) in app.rows" :key="app.active + r.customer + (r.bad ? '!' : '') + i" class="q-rise" :class="{ bad: r.bad, sel: i === app.selected }">
                <td v-for="c in app.cols" :key="c" :class="{ fresh: c === app.fresh, num: c === 'amount' }">
                  <template v-if="c === 'name' || c === 'customer'"><span :class="{ link: c === 'customer' }">{{ r.customer }}</span></template>
                  <template v-else-if="c === 'amount'">{{ r.amount }}</template>
                  <template v-else><span class="pill" :class="r.state">{{ r.state }}</span></template>
                </td>
              </tr>
            </tbody>
          </table>
          <div v-else-if="app.active" class="q-live__nocols">No fields yet</div>
          <div v-if="app.toast" class="q-live__toast q-rise"><b>{{ app.toast }}</b> · not saved</div>
        </div>
      </div>
    </div>

    <div class="q-live__caption">See what you can build with just <b>12 lines</b></div>
  </div>
</template>

<style scoped>
.q-live { position: relative; display: grid; gap: 14px; width: min(560px, 92vw); text-align: left; }
.dot { width: 9px; height: 9px; border-radius: 50%; }

/* the editor */
.q-live__editor { border-radius: 14px; overflow: hidden; background: var(--q-code-bg, #0f1320); border: 1px solid rgba(167, 139, 250, 0.25); box-shadow: 0 30px 80px -24px rgba(76, 29, 149, 0.55); }
.q-live__bar { display: flex; align-items: center; gap: 6px; padding: 9px 14px; border-bottom: 1px solid rgba(255, 255, 255, 0.06); background: rgba(255, 255, 255, 0.02); }
.q-live__bar .dot { background: #2a3146; }
.q-live__file { margin-left: 8px; color: #8b93ad; font-family: var(--vp-font-family-mono); font-size: 11.5px; }
.q-live__count { margin-left: auto; color: #5f6b85; font-family: var(--vp-font-family-mono); font-size: 11px; }
.q-live__code { margin: 0; height: calc(12 * 1.65em + 24px); padding: 12px 16px; overflow: hidden; white-space: pre; color: #d4d8e6; font-family: var(--vp-font-family-mono); font-size: 12px; line-height: 1.65; }
.q-live__code :deep(.k) { color: #c4a7ff; }
.q-live__code :deep(.t) { color: #7dd3fc; }
.q-live__code :deep(.s) { color: #86efac; }
.q-live__code :deep(.n) { color: #fdba74; }
.q-live__code :deep(.o) { color: #94a3b8; }
.q-live__caret { display: inline-block; width: 2px; height: 1.15em; margin-left: 1px; vertical-align: -0.2em; background: #c4b5fd; animation: q-blink 1s steps(1) infinite; }
.q-live__caret.typing { animation: none; }
@keyframes q-blink { 50% { opacity: 0; } }

/* the app */
.q-live__app { position: relative; border-radius: 12px; overflow: hidden; background: #f8fafc; color: #1f2937; border: 1px solid rgba(15, 23, 42, 0.12); box-shadow: 0 24px 60px -24px rgba(15, 23, 42, 0.6); font-family: Inter, ui-sans-serif, system-ui, sans-serif; font-size: 12px; }
.q-live__appbar { display: flex; align-items: center; gap: 6px; padding: 7px 12px; background: #eef1f5; border-bottom: 1px solid #e2e8f0; }
.q-live__appbar .dot { background: #cbd5e1; }
.q-live__url { margin-left: 8px; font-size: 11px; color: #64748b; }
.q-live__live { margin-left: auto; font-size: 10.5px; font-weight: 600; color: #16a34a; }
.q-live__body { display: flex; height: 196px; }
.q-live__nav { flex: none; width: 104px; padding: 10px 8px; background: #fff; border-right: 1px solid #e2e8f0; }
.q-live__brand { padding: 0 6px 8px; font-weight: 700; color: #4338ca; }
.q-live__item { padding: 4px 8px; border-radius: 6px; color: #334155; transition: background 0.4s, color 0.4s; }
.q-live__item.on { background: #eef2ff; color: #4338ca; font-weight: 600; }
.q-live__item.fresh { background: #ddd6fe; }
.q-live__empty { padding: 4px 6px; font-size: 11px; color: #94a3b8; }
.q-live__main { position: relative; flex: 1; min-width: 0; padding: 10px 12px; overflow: hidden; }
.q-live__tools { display: flex; align-items: center; gap: 6px; margin-bottom: 8px; min-height: 22px; }
.q-live__title { margin-right: auto; font-size: 14px; font-weight: 700; color: #0f172a; }
.q-live__btn { padding: 3px 9px; border-radius: 6px; border: 1px solid #cbd5e1; background: #fff; font-size: 11px; font-weight: 500; color: #334155; transition: transform 0.15s, box-shadow 0.4s; }
.q-live__btn.fresh { box-shadow: 0 0 0 3px #ddd6fe; }
.q-live__btn.press { transform: scale(0.92); background: #eef2ff; }
.q-live__btn--primary { border-color: #4f46e5; background: #4f46e5; color: #fff; font-weight: 600; }
.q-live__grid { width: 100%; border-collapse: collapse; background: #fff; border: 1px solid #e2e8f0; }
.q-live__grid th, .q-live__grid td { padding: 5px 8px; border-bottom: 1px solid #eef1f5; text-align: left; white-space: nowrap; transition: background 0.5s; }
.q-live__grid th { background: #fafafa; font-size: 11px; font-weight: 600; color: #475569; }
.q-live__grid .fresh { background: #ede9fe; }
.q-live__grid td.num { text-align: right; font-variant-numeric: tabular-nums; }
.q-live__grid tr.sel td { background: #eef2ff; }
.q-live__grid tr.bad td { background: #fef2f2; color: #b91c1c; }
.link { color: #4338ca; }
.pill { padding: 1px 7px; border-radius: 999px; font-size: 10.5px; font-weight: 600; transition: background 0.4s, color 0.4s; }
.pill.Draft { background: #f1f5f9; color: #475569; }
.pill.Confirmed { background: #e0e7ff; color: #3730a3; }
.q-live__nocols { padding: 18px 0; color: #94a3b8; font-size: 11.5px; }
.q-live__toast { position: absolute; left: 12px; right: 12px; bottom: 10px; padding: 7px 10px; border-radius: 8px; background: #fef2f2; border: 1px solid #fecaca; color: #991b1b; font-size: 11.5px; }

.q-live__caption { text-align: center; color: var(--vp-c-text-2); font-size: 13px; }
.q-live__caption b { color: var(--vp-c-text-1); }

/* what appears, appears softly (CSS animations: they run on their own, with no frame callbacks) */
.q-pop { animation: q-pop 0.35s ease-out both; }
.q-rise { animation: q-rise 0.35s ease-out both; }
@keyframes q-pop { from { opacity: 0; transform: translateX(-6px); } }
@keyframes q-rise { from { opacity: 0; transform: translateY(4px); } }

@media (min-width: 960px) { .q-live { width: min(560px, calc(100vw - 690px)); } }
@media (max-width: 959px) { .q-live { width: 100%; } }
@media (max-width: 640px) {
  .q-live__code { font-size: 11px; padding: 10px 12px; }
  .q-live__nav { width: 82px; }
  .q-live__grid th, .q-live__grid td { padding: 5px 6px; }
}
</style>
