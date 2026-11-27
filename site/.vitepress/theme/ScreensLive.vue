<script setup lang="ts">
// The home, "The screens": a screen is built by dragging components onto the app's grid, and each
// drop is written back to the screen's file. The file at the end is the page's test project's
// interfaces/sales/desk.qdi (it checks with the model): keep them the same. The values are those
// of its `demo` fixture. Off screen it pauses; with reduced motion it shows the end.
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref } from "vue";

type Part = "kpi" | "board" | "button";
const HEAD = ['app "Order desk":', '    page desk "Desk":'];
const ADDS: Record<Part, string[]> = {
  kpi: [
    "        row height 3:",
    '            kpi k_margin "Margin" width 8:',
    "                source: Order.where(o => o.state in {Confirmed, Shipped}).sum(o => o.margin)",
    "                format: money",
  ],
  board: [
    "",
    "        row height 8:",
    '            view v_board "Orders by state" width 16:',
    "                of: Order",
    "                type: kanban",
    "                select: order",
    "                by: state",
    "                cards: customer, revenue, margin",
    "                edit: move",
  ],
  button: [
    "            column width 8:",
    "                button b_confirm height 2:",
    '                    label: "Confirm the selected order"',
    "                    run: @order.confirm()",
  ],
};
const ORDER: Part[] = ["kpi", "board", "button"];
const CHIP: Record<Part, string> = { kpi: "KPI", board: "View", button: "Button" };

const file = ref<string[]>([...HEAD]);
const fresh = ref(0); // how many of the last lines were just written back
const state = reactive({
  placed: [] as Part[],
  over: "" as Part | "",
  pressed: "" as Part | "",
  ghost: "" as Part | "",
  cursor: { x: 70, y: 40, on: false, down: false },
  ghostAt: { x: 0, y: 0 },
});

const canvas = ref<HTMLElement | null>(null);
const chips = ref<Record<string, HTMLElement | null>>({});
const slots = ref<Record<string, HTMLElement | null>>({});
function at(el: HTMLElement | null | undefined) {
  const c = canvas.value?.getBoundingClientRect();
  const r = el?.getBoundingClientRect();
  if (!c || !r) return { x: 0, y: 0 };
  return { x: r.left - c.left + Math.min(r.width / 2, 40), y: r.top - c.top + Math.min(r.height / 2, 22) };
}

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
  file.value = [...HEAD];
  fresh.value = 0;
  Object.assign(state, { placed: [], over: "", pressed: "", ghost: "" });
  state.cursor = { x: 70, y: 40, on: false, down: false };
}

async function drop(part: Part) {
  state.cursor.on = true;
  state.cursor = { ...state.cursor, ...at(chips.value[part]) };
  await sleep(750);
  state.pressed = part;
  state.cursor.down = true;
  state.ghost = part;
  state.ghostAt = at(chips.value[part]);
  await sleep(250);
  state.over = part;
  await nextTick();
  const to = at(slots.value[part]);
  state.cursor = { ...state.cursor, ...to };
  state.ghostAt = to;
  await sleep(850);
  state.cursor.down = false;
  state.pressed = "";
  state.ghost = "";
  state.over = "";
  state.placed.push(part);
  await sleep(350);
  // the drop is written back to the file
  for (const line of ADDS[part]) {
    file.value.push(line);
    fresh.value = ADDS[part].length;
    await sleep(70);
  }
  await sleep(1300);
  fresh.value = 0;
}

async function run() {
  while (alive) {
    reset();
    await sleep(900);
    for (const p of ORDER) await drop(p);
    state.cursor.on = false;
    await sleep(4500);
  }
}

// colors: the first word of a line is its key, then names, strings, numbers, @selections
const esc = (s: string) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
const TOKENS = /("(?:[^"\\]|\\.)*")|(@\w+)|\b([A-Z]\w*)\b|\b(\d+)\b/g;
function paint(line: string) {
  const m0 = /^(\s*)(\w+)/.exec(line);
  let out = "", rest = line;
  if (m0) { out = `${m0[1]}<span class="k">${m0[2]}</span>`; rest = line.slice(m0[0].length); }
  let last = 0;
  for (const m of rest.matchAll(TOKENS)) {
    out += esc(rest.slice(last, m.index));
    const cls = m[1] ? "s" : m[2] ? "n" : m[3] ? "t" : "n";
    out += `<span class="${cls}">${esc(m[0])}</span>`;
    last = m.index! + m[0].length;
  }
  return out + esc(rest.slice(last));
}
const painted = computed(() => file.value.map(paint));
const has = (p: Part) => state.placed.includes(p);

const root = ref<HTMLElement | null>(null);
let io: IntersectionObserver | null = null;
onMounted(() => {
  if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
    file.value = [...HEAD, ...ORDER.flatMap((p) => ADDS[p])];
    state.placed = [...ORDER];
    return;
  }
  io = new IntersectionObserver(([e]) => { visible = e.isIntersecting; if (visible && wake) wake(); });
  if (root.value) io.observe(root.value);
  run();
});
onBeforeUnmount(() => { alive = false; io?.disconnect(); });
</script>

<template>
  <div ref="root" class="q-scr" role="img" aria-label="A screen built by dragging components, and its file written back line by line">
    <div ref="canvas" class="q-scr__canvas">
      <div class="q-scr__bar">
        <span class="q-scr__title">Order desk</span>
        <span class="q-scr__mode">Edit</span>
        <span class="q-scr__palette">
          <span v-for="p in ['KPI', 'Chart', 'View', 'Button', 'Form']" :key="p"
            :ref="(el) => { const part = (Object.keys(CHIP) as Part[]).find((k) => CHIP[k] === p); if (part) chips[part] = el as HTMLElement }"
            class="q-scr__chip" :class="{ pressed: CHIP[state.pressed as Part] === p }">{{ p }}</span>
        </span>
      </div>
      <div class="q-scr__grid">
        <div :ref="(el) => (slots.kpi = el as HTMLElement)" class="q-scr__slot q-scr__slot--kpi" :class="{ over: state.over === 'kpi' }">
          <div v-if="has('kpi')" class="q-scr__kpi q-pop">
            <span>Margin</span><b>2,396 €</b>
          </div>
        </div>
        <div :ref="(el) => (slots.board = el as HTMLElement)" class="q-scr__slot q-scr__slot--board" :class="{ over: state.over === 'board' }">
          <div v-if="has('board')" class="q-scr__board q-pop">
            <div class="q-scr__boardtitle">Orders by state</div>
            <div class="q-scr__cols">
              <div class="q-scr__col"><i>Draft</i><span>Rossi Srl<em>2,970 €</em></span><span>Bianchi SpA<em>1,550 €</em></span></div>
              <div class="q-scr__col"><i>Confirmed</i><span class="sel">Rossi Srl<em>2,876 €</em></span><span>Verdi &amp; Figli<em>1,384 €</em></span></div>
              <div class="q-scr__col"><i>Shipped</i><span>Bianchi SpA<em>1,920 €</em></span></div>
            </div>
          </div>
        </div>
        <div :ref="(el) => (slots.button = el as HTMLElement)" class="q-scr__slot q-scr__slot--button" :class="{ over: state.over === 'button' }">
          <div v-if="has('button')" class="q-scr__button q-pop">Confirm the selected order</div>
        </div>
      </div>
      <div v-if="state.ghost" class="q-scr__ghost" :style="{ transform: `translate(${state.ghostAt.x}px, ${state.ghostAt.y}px)` }">{{ CHIP[state.ghost] }}</div>
      <svg v-show="state.cursor.on" class="q-scr__cursor" :class="{ down: state.cursor.down }" :style="{ transform: `translate(${state.cursor.x}px, ${state.cursor.y}px)` }" viewBox="0 0 16 20" aria-hidden="true">
        <path d="M1 1l13 9-6 1.2 3.6 6.6-2.4 1.2-3.6-6.6L1 16z" fill="#171717" stroke="#fff" stroke-width="1.2" stroke-linejoin="round" />
      </svg>
    </div>

    <div class="q-scr__file">
      <div class="q-scr__filebar">
        <span class="q-scr__name">interfaces/sales/desk.qdi</span>
        <span class="q-scr__saved" :class="{ on: fresh > 0 }">written back</span>
      </div>
      <pre class="q-scr__code"><code><template v-for="(l, i) in painted" :key="i"><span class="q-scr__line" :class="{ fresh: i >= painted.length - fresh }" v-html="l || ' '" />{{ "\n" }}</template></code></pre>
    </div>
  </div>
</template>

<style scoped>
.q-scr { display: grid; gap: 12px; margin-bottom: 14px; }

/* the app, in edit mode */
.q-scr__canvas { position: relative; overflow: hidden; border-radius: var(--q-radius, 8px); background: #fafafa; border: 1px solid var(--q-line); color: #262626; font-family: var(--vp-font-family-base); font-size: 12px; }
.dark .q-scr__canvas { border-color: transparent; }
.q-scr__bar { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; padding: 8px 12px; background: #fff; border-bottom: 1px solid #ececec; }
.q-scr__title { font-size: 13.5px; font-weight: 700; color: #171717; }
.q-scr__mode { padding: 1px 7px; border-radius: 4px; background: #fde047; color: #171717; font-size: 10.5px; font-weight: 500; }
.q-scr__palette { display: inline-flex; flex-wrap: wrap; gap: 5px; margin-left: auto; }
.q-scr__chip { padding: 3px 9px; border-radius: 4px; border: 1px dashed #a3a3a3; background: #fff; color: #262626; font-size: 11px; font-weight: 500; transition: transform 0.15s, background 0.15s; }
.q-scr__chip.pressed { transform: scale(0.92); background: #fef08a; }
.q-scr__grid { display: grid; grid-template-columns: repeat(24, minmax(0, 1fr)); grid-template-rows: 64px 172px; gap: 8px; padding: 10px; background-image: radial-gradient(#e5e5e5 1px, transparent 1px); background-size: 12px 12px; }
.q-scr__slot { border-radius: 6px; border: 1.5px dashed transparent; transition: border-color 0.2s, background 0.2s; min-width: 0; }
.q-scr__slot.over { border-color: #eab308; background: rgba(253, 224, 71, 0.18); }
.q-scr__slot--kpi { grid-column: 1 / span 8; grid-row: 1; }
.q-scr__slot--board { grid-column: 1 / span 16; grid-row: 2; }
.q-scr__slot--button { grid-column: 17 / span 8; grid-row: 2; align-self: start; height: 36px; }
.q-scr__kpi { display: flex; flex-direction: column; justify-content: center; height: 100%; padding: 8px 12px; border-radius: 6px; background: #fff; border: 1px solid #ececec; }
.q-scr__kpi span { font-size: 10.5px; color: #737373; font-weight: 500; }
.q-scr__kpi b { font-size: 19px; color: #171717; font-variant-numeric: tabular-nums; }
.q-scr__board { height: 100%; padding: 8px; border-radius: 6px; background: #fff; border: 1px solid #ececec; }
.q-scr__boardtitle { margin-bottom: 6px; font-size: 11px; font-weight: 700; color: #404040; }
.q-scr__cols { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 6px; }
.q-scr__col { display: grid; align-content: start; gap: 5px; padding: 5px; border-radius: 4px; background: #f5f5f5; }
.q-scr__col i { font-style: normal; font-size: 10px; font-weight: 600; color: #737373; }
.q-scr__col span { display: flex; flex-direction: column; padding: 4px 6px; border-radius: 4px; background: #fff; border: 1px solid #ececec; font-size: 10.5px; font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.q-scr__col span.sel { border-color: #eab308; background: #fefce8; }
.q-scr__col em { font-style: normal; font-weight: 400; color: #737373; font-variant-numeric: tabular-nums; }
.q-scr__button { display: flex; align-items: center; justify-content: center; height: 100%; padding: 0 8px; border-radius: 4px; background: #171717; color: #fff; font-size: 11px; font-weight: 500; text-align: center; }
.q-scr__ghost { position: absolute; left: 0; top: 0; z-index: 2; margin: -10px 0 0 -20px; padding: 4px 10px; border-radius: 4px; background: #171717; color: #fff; font-size: 11px; font-weight: 500; opacity: 0.92; transition: transform 0.8s cubic-bezier(0.4, 0, 0.2, 1); pointer-events: none; }
.q-scr__cursor { position: absolute; left: 0; top: 0; z-index: 3; width: 16px; height: 20px; transition: transform 0.75s cubic-bezier(0.4, 0, 0.2, 1); pointer-events: none; }
.q-scr__cursor.down path { fill: #404040; }

/* the file, written back */
.q-scr__file { border-radius: var(--q-radius, 8px); overflow: hidden; background: var(--q-code-bg); }
.q-scr__filebar { display: flex; align-items: center; gap: 8px; padding: 10px 14px 0; }
.q-scr__name { color: var(--q-c); font-family: var(--vp-font-family-mono); font-size: 11.5px; }
.q-scr__saved { margin-left: auto; color: var(--q-ok); font-family: var(--vp-font-family-mono); font-size: 11px; opacity: 0; transition: opacity 0.3s; }
.q-scr__saved.on { opacity: 1; }
.q-scr__code { margin: 0; height: calc(19 * 1.6em + 20px); padding: 10px 0; overflow: auto hidden; color: var(--q-code-fg); font-family: var(--vp-font-family-mono); font-size: 11.5px; line-height: 1.6; }
.q-scr__code code { display: inline-block; min-width: 100%; }
.q-scr__line { display: inline-block; min-width: 100%; padding: 0 14px; transition: background 1s; }
.q-scr__line.fresh { background: var(--q-hl-soft); box-shadow: inset 2px 0 var(--q-hl); }
.q-scr__code :deep(.k) { color: var(--q-k); font-weight: 600; }
.q-scr__code :deep(.t) { color: var(--q-t); }
.q-scr__code :deep(.s) { color: var(--q-s); }
.q-scr__code :deep(.n) { color: var(--q-n); }

.q-pop { animation: q-pop 0.35s ease-out both; }
@keyframes q-pop { from { opacity: 0; transform: scale(0.96); } }

@media (max-width: 640px) {
  .q-scr__grid { grid-template-rows: 60px 142px 36px; }
  .q-scr__col span:nth-of-type(n + 2) { display: none; }
  .q-scr__slot--kpi { grid-column: 1 / span 12; }
  .q-scr__slot--board { grid-column: 1 / -1; }
  .q-scr__slot--button { grid-column: 1 / -1; grid-row: 3; }
  .q-scr__code { font-size: 10.5px; }
}
</style>
