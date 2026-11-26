---
layout: home
title: The business programming language

hero:
  name: Quadra
  text: The business programming language
  tagline: '<strong>Readable. Provable. Traceable.</strong><br>A programming language that turns a few files of business rules into a complete ERP system.'
  actions:
    - theme: brand
      text: Start the tutorial
      link: /tutorial/
    - theme: alt
      text: Try it in your browser
      link: /sandbox
    - theme: alt
      text: Read the guide
      link: /guide/introduction

features:
  - icon: 🔎
    title: Every number explains itself
    details: Ask any value where it comes from and who can change it. The answer comes from the code.
  - icon: ƒ
    title: Never stale
    details: Totals and balances are declared once and recomputed in the same transaction. No triggers, no caches.
  - icon: 🧊
    title: Documents that stay closed
    details: A confirmed order keeps its values. The compiler flags what would silently stop updating.
  - icon: 🛟
    title: Schema changes with a dry run
    details: Migrations run first on a copy of the live data. If the data doesn't fit, nothing is written.
  - icon: 🔐
    title: Permissions you can't forget
    details: Written once in the model, enforced by the API, the app and every action.
  - icon: ✅
    title: Numbers an auditor can sign
    details: Change anything a certified metric depends on, and the build fails until it is approved again.
---

<div class="q-section">

<div class="q-eyebrow">One model</div>

## Write what your business is. Everything else follows.

<p class="q-lead">A Quadra project is a handful of <code>.qd</code> files. From them the platform derives the database, the API, the app and the audit trail.</p>

<div class="q-split">

```quadra
entity Product:
    name: Text
    price: Money<EUR>

entity Order:
    customer: Text
    lines: [OrderLine]
    total = lines.sum(l => l.subtotal)

entity OrderLine:
    order: Order parent
    product: Product
    qty: Int where > 0 default 1
    subtotal = product.price * qty

role Clerk:
    read Product
    read, create, edit Order
```

<div class="q-cards">
  <div class="q-card"><h4>Database</h4><p>Tables on SQLite or Postgres. Every value keeps its history and its cause.</p></div>
  <div class="q-card"><h4>API</h4><p>Authenticated and idempotent, with a live stream of changes.</p></div>
  <div class="q-card"><h4>App</h4><p>A fast grid with documents, lines, actions and validation.</p></div>
  <div class="q-card"><h4>Tooling</h4><p>Hot reload, an in-browser IDE and an editor extension.</p></div>
</div>

</div>
</div>

<div class="q-section">

<div class="q-eyebrow">Only with a language</div>

## What a framework can't give you

<p class="q-lead">A framework runs your business logic. Quadra understands it, so it can explain it, check it and certify it. Every example below is real output.</p>

<div class="q-proofs">

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">01 · Lineage</div>

### Ask any number where it comes from

What a value depends on, down to the stored fields, and every place that can write them. Derived from the code, so it never drifts.

</div>
<div class="q-proof__code">

<pre class="q-term q-term--tree"><span class="c">$ quadra trace Customer.open_total</span>

<span class="n">Customer.open_total</span> : (inferred)    <span class="s">formula</span> <span class="d">· main.qd:10</span><span class="q-b">1</span>
  = <span class="f">orders.where(o =&gt; o.state is Confirmed).sum(o =&gt; o.total)</span>

<span class="h">Depends on</span><span class="q-b">2</span>
  <span class="n">Customer.orders</span>  <span class="s">stored</span>
  <span class="n">Order.state</span>  <span class="s">stored</span> <span class="d">· default Draft</span>
  <span class="n">Order.total</span>  = <span class="f">lines.sum(l =&gt; l.subtotal)</span>
    <span class="n">Order.lines</span>  <span class="s">stored</span>
    <span class="n">OrderLine.subtotal</span>  = <span class="f">(product.price * qty).round()</span>
      <span class="n">OrderLine.product</span>  <span class="s">stored</span>
      <span class="n">OrderLine.qty</span>  <span class="s">stored</span> <span class="d">· default 1</span>
      <span class="n">Product.price</span>  <span class="s">stored</span>

<span class="h">Stored values underneath, and who writes them</span><span class="q-b">3</span>
  <span class="n">Order.state</span>
      <span class="u">flow Draft → Confirmed by confirm</span>            <span class="d">main.qd:26</span>
  <span class="n">Product.price</span>
      <span class="u">user, through forms and API</span>
  …</pre>

<ol class="q-notes">
  <li><span class="q-b">1</span><span>The value, its formula, and where it is declared.</span></li>
  <li><span class="q-b">2</span><span>Everything it depends on: <b style="color:#fcd34d">formulas</b>, down to the <b style="color:#c4a7ff">stored</b> fields.</span></li>
  <li><span class="q-b">3</span><span>For each stored field, <b style="color:#86efac">who can change it</b>: here a flow, and users through forms and the API.</span></li>
</ol>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">02 · Never stale</div>

### Change a price, and only what depends on it is recomputed

The compiler knows the path from a product's price to a customer's open total. A write recomputes exactly that, in the same transaction. No triggers, no cache to invalidate.

</div>
<div class="q-proof__code">

```quadra
entity Customer:
    orders: [Order] via customer
    open_total = orders.where(o => o.state is Confirmed).sum(o => o.total)

entity Order:
    lines: [OrderLine]
    total = lines.sum(l => l.subtotal)

entity OrderLine:
    order: Order parent
    product: Product
    qty: Int where > 0 default 1
    subtotal = (product.price * qty).round()
```

<div class="q-label">When a product's price changes</div>
<div class="q-path"><span class="stored">Product.price</span><i>→</i><span>OrderLine.subtotal</span><i>→</i><span>Order.total</span><i>→</i><span>Customer.open_total</span></div>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">03 · Causes</div>

### Every record knows why it exists

Every write carries its cause. A purchase order created by a reaction can tell you which sale triggered it.

</div>
<div class="q-proof__code">

```quadra
on ReorderRule becomes to_order > 0 as r converges:
    let po = PurchaseOrder.find(p => p.vendor == r.vendor and p.state is Draft)
             ?? PurchaseOrder(vendor=r.vendor, warehouse=r.warehouse, notes="Automatic reorder")
    PurchaseLine(order=po, product=r.product, qty=r.to_order.round_up())
```

<pre class="q-term q-term--tree"><span class="c">why(po)</span>

<span class="n">PurchaseOrder PO/2026/00001</span> created
  <span class="d">because</span> <span class="f">on ReorderRule becomes to_order &gt; 0</span>  <span class="u">(giulia, 2026-11-02 09:00)</span><span class="q-b">1</span>
  <span class="d">because</span> <span class="f">on SalesOrder enters Confirmed</span>  <span class="u">(giulia, 2026-11-02 09:00)</span><span class="q-b">2</span>
  <span class="d">because</span> <span class="s">SalesOrder.confirm</span>  <span class="u">(giulia, 2026-11-02 09:00)</span><span class="q-b">3</span></pre>

<ol class="q-notes">
  <li><span class="q-b">1</span><span>The reaction that created it: there was stock to reorder.</span></li>
  <li><span class="q-b">2</span><span>What set that off: a sales order was confirmed, and the forecast dropped.</span></li>
  <li><span class="q-b">3</span><span>Where it all started: <b style="color:#86efac">Giulia</b> clicked Confirm. Read it bottom up for the whole story.</span></li>
</ol>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">04 · Frozen documents</div>

### A confirmed order keeps its word

Confirmed orders freeze. The compiler finds the values that must keep moving, like the invoices that arrive later.

</div>
<div class="q-proof__code">

```quadra
entity Order:
    lines: [OrderLine]
    invoices: [Invoice] via order
    state: enum(Draft, Confirmed, Delivered) default Draft

    total = lines.sum(l => l.amount)
    invoiced = invoices.sum(i => i.amount)

    frozen when state in {Confirmed, Delivered}
```

<pre class="q-term"><span class="w">warning</span>: <span class="x">`invoiced`</span><span class="q-b">1</span> reads <span class="n">Invoice</span> (via order): records that point to the Order and come after it is frozen.<span class="q-b">2</span> On a frozen Order it would keep the value it had when frozen; if it should follow them, declare <span class="ok">`live invoiced`</span><span class="q-b">3</span></pre>

<ol class="q-notes">
  <li><span class="q-b">1</span><span>The value at risk: how much of the order has been invoiced.</span></li>
  <li><span class="q-b">2</span><span>The catch: invoices arrive after the order is confirmed, so a frozen value would stay at zero.</span></li>
  <li><span class="q-b">3</span><span>The fix, one word: this value keeps moving on a frozen order.</span></li>
</ol>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">05 · Safe migrations</div>

### Change the schema on a Friday

Migrations are code, and they run first on a copy of the live data. If the data doesn't fit the new model, nothing is written.

</div>
<div class="q-proof__code">

```quadra
migration customs:
    reason: "Customs codes are required for shipping outside the EU"

    rename Product.product_kind -> Product.type
    convert Product.type from old.Text:
        "merce"    => Storable
        "consumo"  => Consumable
        "servizio" => Service
        other      => fail f"Unknown product kind: {other}"

    backfill Product.hs_code:
        for p in Product.all():
            if let code = CustomsCode.find(c => c.product_code == p.code)?.code:
                p.hs_code = code
            else:
                p.hs_code = provisional("00000000", reason="To classify", until=2027-01-31)

    verify:
        Product.count(p => p.hs_code.is_provisional) < 2
```

<pre class="q-term q-term--tree"><span class="c">$ quadra db migrate --dry-run</span>
  <span class="h">migration customs</span>
    <span class="s">rename</span> <span class="n">Product.product_kind → Product.type</span>: 3 values <span class="d">(the history keeps the old name)</span><span class="q-b">1</span>
    <span class="s">convert</span> <span class="n">Product.type</span>: 3 values
    <span class="s">backfill</span> <span class="n">Product.hs_code</span>: 3 values, <span class="w">1 provisional</span><span class="q-b">2</span>
    <span class="s">verify</span>: <span class="ok">ok</span>
  <span class="d">rename column product.product_kind → type</span><span class="q-b">3</span>
  <span class="d">copy table product</span>
  <span class="d">every Product: 3 new or changed rules</span>
<span class="ok">✓ dry run: the data satisfies the new model; nothing written</span><span class="q-b">4</span></pre>

<ol class="q-notes">
  <li><span class="q-b">1</span><span>Each step says how many values it touches. The history keeps the old name.</span></li>
  <li><span class="q-b">2</span><span>One product had no customs code: it gets a placeholder with a reason and a deadline.</span></li>
  <li><span class="q-b">3</span><span>The changes it would make to the database tables.</span></li>
  <li><span class="q-b">4</span><span>Checked on a copy of the live data: nothing is written until everything passes.</span></li>
</ol>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">06 · Permissions</div>

### Permissions written once, enforced everywhere

Rules are expressions over the model. The grid, the API, the actions and the reactions all apply the same ones.

</div>
<div class="q-proof__code">

```quadra
role Salesperson is Employee:
    read, create, edit SalesOrder where o => o.salesperson == me
    run SalesOrder.send, SalesOrder.confirm where o => o.salesperson == me
    hide Product.standard_cost, Product.avg_cost

# for everybody, whatever their roles
deny run PurchaseOrder.approve where o => o.created_by == me
```

<pre class="q-term"><span class="e">permission denied</span>: <span class="f">deny run PurchaseOrder.approve</span><span class="q-b">1</span> <span class="u">(giulia)</span><span class="q-b">2</span></pre>

<ol class="q-notes">
  <li><span class="q-b">1</span><span>The rule that refused it: nobody approves a purchase order they created.</span></li>
  <li><span class="q-b">2</span><span>Who tried. It holds even for someone who has every role.</span></li>
</ol>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">07 · Certified numbers</div>

### A number the CFO signed stays signed

A certified metric is bound to the fingerprint of everything it depends on. Change a formula three modules away, and the build fails until someone approves it again.

</div>
<div class="q-proof__code">

```quadra
metric revenue(company: Company, period: DateRange) -> Money<company.currency>:
    label: "Net revenue"
    owner: "Finance"
    value = Invoice
        .where(i => i.company == company and i.direction is Out and i.state is Posted and i.date in period)
        .sum(i => convert(i.untaxed * i.sign, to=company.currency, rate=i.rate).round())
```

<pre class="q-term"><span class="c">$ quadra check</span>
<span class="e">error</span>: the definition of <span class="x">`revenue`</span><span class="q-b">1</span> changed since it was certified by <span class="u">m.bianchi (CFO) on 2026-11-21</span><span class="q-b">2</span>
<span class="e">error</span>: the definition of <span class="x">`gross_margin`</span> changed since it was certified by <span class="u">m.bianchi (CFO) on 2026-11-21</span>
<span class="e">error</span>: the definition of <span class="x">`dso`</span> changed since it was certified by <span class="u">l.verdi (Tesoreria) on 2026-11-21</span>
<span class="e">error</span>: the definition of <span class="x">`receivables_subledger`</span> changed since it was certified by <span class="u">m.bianchi (CFO) on 2026-11-21</span>
<span class="e">✗ 4 errors, 0 warnings in 20 modules</span><span class="q-b">3</span></pre>

<ol class="q-notes">
  <li><span class="q-b">1</span><span>The metric whose meaning changed. Its own formula is untouched: something it depends on changed in another module.</span></li>
  <li><span class="q-b">2</span><span>Who signed the old definition, and when.</span></li>
  <li><span class="q-b">3</span><span>The build stops until someone signs again with <code>quadra certify</code>.</span></li>
</ol>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">08 · Logic errors</div>

### Mistakes that cost money don't compile

Currencies and units are types, rounding is explicit, formulas can't depend on themselves, and a state nobody can reach is an error.

</div>
<div class="q-proof__code">

```quadra
entity Order:
    lines: [OrderLine]
    state: enum(Draft, Confirmed, Shipped, Archived) default Draft
    discount: Percent default 0%

    total = lines.sum(l => l.amount) - rebate
    rebate = total * discount

    flow state:
        Draft -> Confirmed by confirm
        Confirmed -> Shipped by ship

entity OrderLine:
    order: Order parent
    qty: Qty<kg>
    price: Price<EUR, kg>
    amount: Money<EUR> = qty * price * 1.1
```

<pre class="q-term"><span class="e">error</span>: formula cycle: <span class="n">Order.rebate → Order.total → Order.rebate</span><span class="q-b">1</span>
       <span class="d">(break it with a `default` snapshot, or make one value stored)</span>
<span class="w">warning</span>: state <span class="x">`Archived`</span> of <span class="n">`Order.state`</span> can never be reached<span class="q-b">2</span>
<span class="e">error</span>: <span class="n">Exact&lt;Money&lt;EUR&gt;&gt;</span> must be rounded with <span class="ok">.round()</span> before it is stored as <span class="n">Money&lt;EUR&gt;</span><span class="q-b">3</span></pre>

<ol class="q-notes">
  <li><span class="q-b">1</span><span>The rebate needs the total, and the total needs the rebate.</span></li>
  <li><span class="q-b">2</span><span>No transition leads to Archived: a state nobody can ever use.</span></li>
  <li><span class="q-b">3</span><span>Multiplying by 1.1 creates fractions of a cent. You decide how to round; the compiler makes sure you do.</span></li>
</ol>

</div>
</div>

</div>
</div>

<div class="q-section">

<div class="q-eyebrow">Learn by building</div>

## From a first entity to a working business app

<p class="q-lead">The tutorial grows one small shop step by step. Every step runs in the sandbox, and every snippet compiles.</p>

<ol class="q-steps">
  <li><b>Entities and types</b>: fields, money, quantities, dates</li>
  <li><b>Relations and formulas</b>: references, documents with lines, rollups</li>
  <li><b>Rules and flows</b>: checks, state machines, actions, frozen documents</li>
  <li><b>Tests</b>: business scenarios that run in milliseconds</li>
  <li><b>Permissions and ledgers</b>: roles, stock and accounting balances</li>
  <li><b>Database, API and app</b>: run it, change it, migrate it safely</li>
</ol>

<a class="q-sandbox-link" href="/tutorial/">Start the tutorial →</a>

</div>

<div class="q-cta">

## Try it now. Nothing to install.

<p class="q-lead" style="margin: 0 auto 18px">The sandbox runs the compiler, the engine and the web app in your browser. Edit the code, watch the app change, run the tests, then download the project and run it with <code>quadra dev</code>.</p>

<SandboxLink project="welcome" label="Open the Sandbox" />

</div>
