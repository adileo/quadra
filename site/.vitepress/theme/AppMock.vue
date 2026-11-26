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
  width: 100%; border-radius: 12px; overflow: hidden; text-align: left;
  background: #f8fafc; color: #1f2937; border: 1px solid rgba(15, 23, 42, 0.12);
  box-shadow: 0 30px 70px -24px rgba(15, 23, 42, 0.55);
  font-family: Inter, ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; font-size: 12.5px; line-height: 1.4;
}
.q-app__bar { display: flex; align-items: center; gap: 6px; padding: 8px 12px; background: #eef1f5; border-bottom: 1px solid #e2e8f0; }
.q-app__bar .dot { width: 9px; height: 9px; border-radius: 50%; background: #cbd5e1; }
.q-app__url { margin-left: 8px; font-size: 11.5px; color: #64748b; }
.q-app__auto { margin-left: auto; padding: 2px 8px; border-radius: 999px; background: #ede9fe; color: #5b21b6; font-size: 10.5px; font-weight: 600; white-space: nowrap; }
.q-app__body { display: flex; min-height: 0; }
.q-app__nav { flex: none; width: 112px; padding: 10px 8px; background: #fff; border-right: 1px solid #e2e8f0; }
.q-app__brand { padding: 0 6px 8px; font-weight: 700; color: #4338ca; letter-spacing: -0.01em; }
.q-app__group { padding: 4px 6px; font-size: 10px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.05em; color: #64748b; }
.q-app__item { padding: 4px 8px; border-radius: 6px; font-size: 12px; color: #334155; }
.q-app__item--on { background: #eef2ff; color: #4338ca; font-weight: 600; }
.q-app__main { flex: 1; min-width: 0; padding: 10px 12px 12px; overflow-x: auto; }
.q-app__tools { display: flex; align-items: center; gap: 6px; margin-bottom: 8px; }
.q-app__title { margin-right: auto; font-size: 14px; font-weight: 700; color: #0f172a; }
.q-app__btn { padding: 3px 9px; border-radius: 6px; border: 1px solid #cbd5e1; background: #fff; font-size: 11px; font-weight: 500; color: #334155; white-space: nowrap; }
.q-app__btn--primary { border-color: #4f46e5; background: #4f46e5; color: #fff; font-weight: 600; }
.q-app__grid { display: table; width: 100%; margin: 0; border-collapse: collapse; background: #fff; border: 1px solid #e2e8f0; font-size: 12px; }
.q-app__grid tr { background: #fff; border: none; }
.q-app__grid th, .q-app__grid td { padding: 6px 8px; border: none; border-bottom: 1px solid #eef1f5; text-align: left; white-space: nowrap; }
.q-app__grid th { background: #fafafa; font-size: 11px; font-weight: 600; color: #475569; }
.q-app__grid th.f { background: #f1f5f9; font-style: italic; }
.q-app__grid td.num { text-align: right; font-variant-numeric: tabular-nums; }
.q-app__grid td.m { color: #64748b; font-variant-numeric: tabular-nums; }
.q-app__state { padding: 1px 7px; border-radius: 999px; font-size: 10.5px; font-weight: 600; }
.s-Draft { background: #f1f5f9; color: #475569; }
.s-Confirmed { background: #e0e7ff; color: #3730a3; }
.s-Shipped { background: #dcfce7; color: #166534; }
.q-app__sub { margin: 10px 0 5px; font-size: 11px; font-weight: 600; color: #475569; }
.q-app__grid tr.sel td { background: #eef2ff; }
.q-app__toast { display: flex; flex-wrap: wrap; gap: 2px 8px; margin-top: 10px; padding: 8px 10px; border-radius: 8px; background: #fef2f2; border: 1px solid #fecaca; color: #991b1b; font-size: 11.5px; }
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
