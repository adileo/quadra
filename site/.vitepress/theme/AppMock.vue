<script setup lang="ts">
// A picture of the app Quadra builds from the orders model of the home (orders.qd): the sidebar of
// entities, the Orders grid, the formulas' columns (grey, italic), the actions of the flow.
// Drawn in the style of the web client (web/src: slate and indigo); the values are those of the
// `demo` fixture of the page's test project. `compact` is the hero's version: only what the hero's code declares (no state, no flow,
// no Customer entity), with the lines of the selected order.
defineProps<{ compact?: boolean }>();

const rows = [
  { customer: "Rossi Srl", date: "2026-11-23", state: "Confirmed", revenue: "2,876.00", margin: "1,188.00" },
  { customer: "Bianchi SpA", date: "2026-11-23", state: "Shipped", revenue: "1,920.00", margin: "690.00" },
  { customer: "Verdi & Figli", date: "2026-11-23", state: "Confirmed", revenue: "1,384.00", margin: "518.00" },
  { customer: "Rossi Srl", date: "2026-11-23", state: "Draft", revenue: "2,970.00", margin: "1,710.00" },
  { customer: "Bianchi SpA", date: "2026-11-23", state: "Draft", revenue: "1,550.00", margin: "600.00" },
];
const lines = [
  { product: "Valve DN50", qty: 40, amount: "2,480.00", margin: "960.00" },
  { product: "Seal kit", qty: 40, amount: "396.00", margin: "228.00" },
];
</script>

<template>
  <div class="q-app" :class="{ 'q-app--compact': compact }" role="img" aria-label="The orders app Quadra builds from the model">
    <div class="q-app__bar">
      <span class="dot" /><span class="dot" /><span class="dot" />
      <span class="q-app__url">Orders</span>
      <span v-if="!compact" class="q-app__auto">built from orders.qd</span>
    </div>
    <div class="q-app__body">
      <nav class="q-app__nav">
        <div class="q-app__brand">Quadra</div>
        <div class="q-app__group">Data</div>
        <div v-if="!compact" class="q-app__item">Customer</div>
        <div class="q-app__item q-app__item--on">Order</div>
        <div class="q-app__item">OrderLine</div>
        <div class="q-app__item">Product</div>
      </nav>
      <div class="q-app__main">
        <div class="q-app__tools">
          <span class="q-app__title">Order</span>
          <span class="q-app__btn q-app__btn--primary">+ New</span>
          <template v-if="!compact"><span class="q-app__btn">Confirm</span><span class="q-app__btn">Ship</span></template>
        </div>
        <table class="q-app__grid">
          <thead>
            <tr>
              <th>customer</th>
              <th v-if="!compact" class="c-date">date</th>
              <th v-if="!compact">state</th>
              <th class="f">revenue</th>
              <th class="f">margin</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="(r, i) in rows" :key="i" :class="{ sel: compact && i === 0 }">
              <td>{{ r.customer }}</td>
              <td v-if="!compact" class="m c-date">{{ r.date }}</td>
              <td v-if="!compact"><span class="q-app__state" :class="'s-' + r.state">{{ r.state }}</span></td>
              <td class="num">{{ r.revenue }} €</td>
              <td class="num">{{ r.margin }} €</td>
            </tr>
          </tbody>
        </table>
        <div v-if="compact" class="q-app__sub">Lines of Rossi Srl</div>
        <table v-if="compact" class="q-app__grid">
          <thead><tr><th>product</th><th>qty</th><th class="f">amount</th><th class="f">margin</th></tr></thead>
          <tbody>
            <tr v-for="(l, i) in lines" :key="i">
              <td>{{ l.product }}</td><td class="num">{{ l.qty }}</td><td class="num">{{ l.amount }} €</td><td class="num">{{ l.margin }} €</td>
            </tr>
          </tbody>
        </table>
        <div class="q-app__toast"><b>Not enough stock</b><span>10 × Pump P200 · the line was not saved</span></div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.q-app {
  width: 100%; border-radius: 8px; overflow: hidden; text-align: left;
  background: #fafafa; color: #262626; border: 1px solid #e5e5e5;
  font-family: var(--vp-font-family-base); font-size: 12.5px; line-height: 1.4;
}
.q-app__bar { display: flex; align-items: center; gap: 6px; padding: 8px 12px; background: #f5f5f5; border-bottom: 1px solid #ececec; }
.q-app__bar .dot { display: none; }
.q-app__url { font-family: var(--vp-font-family-mono); font-size: 11.5px; color: #8a8a8a; }
.q-app__auto { margin-left: auto; padding: 1px 7px; border-radius: 4px; background: #fde047; color: #171717; font-size: 10.5px; font-weight: 500; white-space: nowrap; }
.q-app__body { display: flex; min-height: 0; }
.q-app__nav { flex: none; width: 112px; padding: 10px 8px; background: #fff; border-right: 1px solid #ececec; }
.q-app__brand { padding: 0 6px 8px; font-weight: 700; color: #171717; letter-spacing: -0.01em; }
.q-app__group { padding: 4px 6px; font-size: 10px; font-weight: 500; color: #8a8a8a; }
.q-app__item { padding: 4px 8px; border-radius: 4px; font-size: 12px; color: #404040; }
.q-app__item--on { background: #f0f0f0; color: #171717; font-weight: 600; }
.q-app__main { flex: 1; min-width: 0; padding: 10px 12px 12px; overflow-x: auto; }
.q-app__tools { display: flex; align-items: center; gap: 6px; margin-bottom: 8px; }
.q-app__title { margin-right: auto; font-size: 14px; font-weight: 700; color: #171717; }
.q-app__btn { padding: 3px 9px; border-radius: 4px; border: 1px solid #d4d4d4; background: #fff; font-size: 11px; font-weight: 500; color: #404040; white-space: nowrap; }
.q-app__btn--primary { border-color: #171717; background: #171717; color: #fff; }
.q-app__grid { display: table; width: 100%; margin: 0; border-collapse: collapse; background: #fff; border: 1px solid #ececec; font-size: 12px; }
.q-app__grid tr { background: #fff; border: none; }
.q-app__grid th, .q-app__grid td { padding: 6px 8px; border: none; border-bottom: 1px solid #f0f0f0; text-align: left; white-space: nowrap; }
.q-app__grid th { background: #fafafa; font-size: 11px; font-weight: 600; color: #525252; }
.q-app__grid th.f { background: #f5f5f5; font-style: italic; }
.q-app__grid td.num { text-align: right; font-variant-numeric: tabular-nums; }
.q-app__grid td.m { color: #737373; font-variant-numeric: tabular-nums; }
.q-app__state { padding: 1px 6px; border-radius: 4px; font-size: 10.5px; font-weight: 500; }
.s-Draft { background: #f0f0f0; color: #525252; }
.s-Confirmed { background: #fef08a; color: #171717; }
.s-Shipped { background: #171717; color: #fff; }
.q-app__sub { margin: 10px 0 5px; font-size: 11px; font-weight: 600; color: #525252; }
.q-app__grid tr.sel td { background: #fefce8; }
.q-app__toast { display: flex; flex-wrap: wrap; gap: 2px 8px; margin-top: 10px; padding: 8px 10px; border-radius: 6px; background: #fef2f2; color: #991b1b; font-size: 11.5px; }
.q-app__toast b { font-weight: 700; }
.q-app--compact { font-size: 11.5px; }
.q-app--compact .q-app__nav { width: 96px; }
.q-app--compact .q-app__grid { font-size: 11px; }
.q-app--compact .q-app__grid th, .q-app--compact .q-app__grid td { padding: 5px 7px; }
@media (max-width: 640px) {
  .q-app__nav { display: none; }
  .c-date { display: none; }
  .q-app__grid th, .q-app__grid td { padding: 5px 6px; }
}
</style>
