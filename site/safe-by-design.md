---
layout: home
title: Safe by Design
description: Why a language is safer than a framework. The checks Quadra makes on the data, the states, the rules and the automations of a business system, with real examples.
pageClass: q-v2 q-safe-page
---

<div class="q-page-intro">

<div class="q-eyebrow">Safe by Design</div>

# Why a language is safer than a framework

<p class="q-lead">A framework runs your code and trusts it. Quadra understands the whole model: the data, the states, the rules and the automations. So it can tell you when the logic is wrong, not only the syntax, before a single record is saved.</p>

<p class="q-lead">Business systems break in the same places: amounts in the wrong currency, state machines with dead ends, automations that set off other automations, and nobody able to say why a number is what it is. Here is what Quadra does about each of them. The messages on this page come from the Quadra tools.</p>

</div>

<div class="q-section">

<div class="q-eyebrow">Short by design</div>

## Fewer lines, fewer places for a bug to hide

<p class="q-lead">A general-purpose framework makes you spell out <em>how</em>: tables, migrations, serializers, validation, status checks, locking, audit tables. Quadra is made for one job, so you write only <em>what</em>: the data, the rules and the flow. The whole order system on the home page is 39 lines.</p>

<div class="q-proofs">

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">The same action, twice</div>

### Ship an order

On the right, shipping an order: first in Quadra, then in a typical web framework.

The framework version is not badly written. It has to say everything: lock the rows, check the status, check the stock, write the audit log by hand, save each field. Every one of those lines is a place where a bug can hide, and every reviewer has to read them all.

In Quadra the flow says when `ship` may run, the type of `stock` says it can't go below zero, the transaction and the history come with every action. What is left is the business.

</div>
<div class="q-proof__code">

<div class="q-label">Quadra · 7 lines</div>

```quadra
stock: Int where >= 0

state: enum(Draft, Confirmed, Shipped) default Draft

flow state:
    Confirmed -> Shipped by ship

action ship:
    for l in lines:
        l.product.stock = l.product.stock - l.qty
```

<div class="q-label">A web framework · 24 lines, and still incomplete</div>

```python
class Order(models.Model):
    class State(models.TextChoices):
        DRAFT = "draft"
        CONFIRMED = "confirmed"
        SHIPPED = "shipped"

    state = models.CharField(max_length=20, choices=State.choices, default=State.DRAFT)

    def ship(self, user):
        with transaction.atomic():
            order = Order.objects.select_for_update().get(pk=self.pk)
            if order.state != Order.State.CONFIRMED:
                raise ValidationError(f"Cannot ship an order in state {order.state}")
            for line in order.lines.select_related("product").select_for_update():
                product = line.product
                if line.qty > product.stock:
                    raise ValidationError("Not enough stock")
                old = product.stock
                product.stock -= line.qty
                product.save(update_fields=["stock"])
                AuditLog.objects.create(user=user, record=product, field="stock",
                                        old=old, new=product.stock, cause=f"order {order.pk} shipped")
            order.state = Order.State.SHIPPED
            order.save(update_fields=["state"])
            AuditLog.objects.create(user=user, record=order, field="state",
                                    old="confirmed", new="shipped", cause="ship")
```

<p class="q-built">Still to write: the permission check, the API endpoint, the form, the migration, and the audit table itself. And nothing stops another piece of code from writing <code>state = "shipped"</code>.</p>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">What you don’t write</div>

### The plumbing comes from the model

Short code is not a trick of syntax. It is short because the language knows what an order, an amount, a state and a history are, so it builds the rest from the model, the same way every time.

Less code also means less for an AI assistant to write, and less for you to check after it.

</div>
<div class="q-proof__code">

| | Framework + ORM + SQL | Quadra |
|---|---|---|
| Schema | migrations, ORM classes, API serializers, forms, kept in sync by hand | one model: tables, API and screens are derived |
| Derived values | computed in code, cached, sometimes stale | formulas, always current |
| Money and units | decimals, currency in another column | `Money<EUR>`, `Qty<kg>`, checked by the compiler |
| Validation | in the controller, the form or a trigger, often only one of them | `check`, `where`: on every write, from any entry point |
| Status changes | `if order.status == …` scattered in the code | `flow` + `action`: only the declared transitions exist |
| Audit | a table someone remembers to write to | every value has a history and a cause |
| Permissions | middleware and filters per endpoint | `role` rules, applied everywhere |

</div>
</div>

</div>

</div>

<div class="q-section">

<div class="q-eyebrow">Caught while you write</div>

## The compiler reads the whole model

<p class="q-lead">These mistakes are shown in the editor as you type, on every save in <code>quadra dev</code>, by <code>quadra check</code> and before every deploy. Nothing runs until they are fixed.</p>

<div class="q-proofs">

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">Types</div>

### Money and units have types

In a framework a price is a decimal: nothing stops you from adding euros to dollars, or kilos to pieces, and the mistake surfaces in a report weeks later.

In Quadra, `Money<EUR>`, `Qty<kg>` and `Price<EUR, kg>` are types. Adding two currencies, comparing an amount with a plain number or forgetting to round is an error at its line.

</div>
<div class="q-proof__code">

```quadra
entity Product:
    price: Money<EUR>
    list_usd: Money<USD>

    total_price = price + list_usd
```

<pre class="q-term"><span class="e">error</span>: cannot add Money&lt;EUR&gt; and Money&lt;USD&gt;
<span class="d">  │</span>
<span class="d">  │</span>     total_price = <span class="x">price + list_usd</span>
<span class="d">  │</span>                   <span class="e">^^^^^^^^^^^^^^^^</span></pre>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">State machines</div>

### States nobody can reach

ERP state machines grow one status at a time. A status gets added, nothing ever leads to it, and it sits in every dropdown and every report for years.

In Quadra a flow is declared, not coded, so the compiler can see all of it: a state that no transition can reach is flagged.

</div>
<div class="q-proof__code">

```quadra
entity Order:
    state: enum(Draft, Confirmed, Shipped, Cancelled) default Draft

    flow state:
        Draft -> Confirmed by confirm
        Confirmed -> Shipped by ship
```

<pre class="q-term"><span class="w">warning</span>: state <span class="x">`Cancelled`</span> of <span class="n">`Order.state`</span> can never be reached</pre>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">State machines</div>

### No shortcuts around the flow

In most systems the status is just a column, and any piece of code can write `status = 'shipped'`, skipping the checks that shipping should run.

In Quadra a state changes only through the actions of its flow. Writing the field directly is an error, wherever it is written.

</div>
<div class="q-proof__code">

```quadra
action ship:
    for l in lines:
        l.product.stock = l.product.stock - l.qty
    state = Draft
```

<pre class="q-term"><span class="e">error</span>: <span class="n">`Order.state`</span> changes only through its flow: run one of its actions (<span class="ok">confirm, ship</span>)</pre>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">State machines</div>

### Every case handled

Add a state to an order, and somewhere a label, a total or a permission still only knows the old ones.

A `match` on a state must cover every state. Add one, and every place that forgot it is listed.

</div>
<div class="q-proof__code">

```quadra
label: Text = match state:
    Draft => "to confirm"
    Confirmed => "to ship"
```

<pre class="q-term"><span class="e">error</span>: non-exhaustive match: missing <span class="x">Shipped</span></pre>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">Automations</div>

### Cascades that would never stop

The real trouble in an ERP is often not one rule, but rules that set off other rules: a stock change triggers a reorder, which changes the stock again, and so on.

Quadra knows what every automation writes and what its condition reads. One that can trigger itself again is refused, until you declare that it converges. At run time, automations that keep changing each other stop the transaction after 20 rounds, and nothing is saved.

</div>
<div class="q-proof__code">

```quadra
on Product becomes stock < 5 as p:
    p.stock = p.stock + 1
```

<pre class="q-term"><span class="e">error</span>: <span class="n">`on Product becomes stock &lt; 5`</span> can trigger itself again: it writes <span class="x">`Product.stock`</span>, which its condition reads. If each run brings the condition back to false, declare it <span class="ok">`converges`</span> (`as p converges:`)</pre>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">Formulas</div>

### Formulas that depend on themselves

A discount that depends on the total, which depends on the discount: in a spreadsheet it is a circular reference warning that people learn to ignore.

Quadra finds the cycle, names every step of it and says how to break it.

</div>
<div class="q-proof__code">

```quadra
revenue = lines.sum(l => l.amount) + discount
discount = revenue * 0.1
```

<pre class="q-term"><span class="e">error</span>: formula cycle: <span class="n">Order.discount → Order.revenue → Order.discount</span> (break it with a `default` snapshot, or make one value stored)</pre>

</div>
</div>

</div>

</div>

<div class="q-section">

<div class="q-eyebrow">Enforced while it runs</div>

## The same rules for every click, API call and automation

<p class="q-lead">Rules live in the model, not in a form or a controller. So they hold no matter where a change comes from.</p>

<div class="q-proofs">

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">Transitions</div>

### Only the declared transitions

An order can't be shipped before it is confirmed: not from the app, not from the API, not from an automation. Each action runs only in the states its flow allows.

</div>
<div class="q-proof__code">

```quadra
let o = Order(customer=anna, lines=[OrderLine(product=pump, qty=2)])
o.ship()
```

<pre class="q-term"><span class="e">Order.ship is not allowed in state Draft</span></pre>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">Transactions</div>

### All or nothing

Every action runs in one transaction, together with everything it sets off. If one rule fails, all of it is undone: no half-confirmed orders, no stock moved for a shipment that didn't happen.

</div>
<div class="q-proof__code">

```quadra
test "an order below cost is not confirmed":
    let pump = Product(name="Pump", cost=20.00 EUR, price=15.00 EUR, stock=10)
    let o = Order(customer=anna, lines=[OrderLine(product=pump, qty=2)])
    expect_error "Below cost":
        o.confirm()
    expect o.state is Draft
```

<pre class="q-term"><span class="c">$ quadra test</span>
<span class="ok">✓</span> an order below cost is not confirmed

<span class="ok">1 passed, 0 failed</span></pre>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">Frozen records</div>

### Facts stay facts

Once an order is confirmed, it is a fact. `frozen when` makes it read-only, and its formulas keep seeing the world as it was that day: tomorrow's price list doesn't change yesterday's order. Corrections go through actions, so they leave a trace.

</div>
<div class="q-proof__code">

```quadra
entity Order:
    state: enum(Draft, Confirmed, Shipped) default Draft
    frozen when state is not Draft
```

<pre class="q-term"><span class="e">Order is frozen (state is not Draft)</span>: <span class="x">`date`</span> cannot change</pre>

</div>
</div>

</div>

</div>

<div class="q-section">

<div class="q-eyebrow">Explained afterwards</div>

## Every value knows where it came from

<p class="q-lead">Nothing in the history is ever overwritten. So any number can be explained, and any change can be checked or taken back.</p>

<div class="q-proofs">

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">Causes</div>

### Why is it this way?

Every write records its cause: the user, the action, the automation, the API request or the migration, and the chain that led to it. `why()` tells the story of any record.

</div>
<div class="q-proof__code">

```quadra
why(pump)
```

<pre class="q-term">Product Pump created
  because <span class="n">Product.stock</span> changed by <span class="s">Order.ship</span>  <span class="u">(anna, 2026-11-25 09:00)</span></pre>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">Dependencies</div>

### What does it depend on?

Before you change a field, see everything a number is made of, and who writes each piece underneath. `quadra trace` answers it for any value.

</div>
<div class="q-proof__code">

<pre class="q-term q-term--tree"><span class="c">$ quadra trace Order.margin</span>
<span class="h">Order.margin</span>    formula · main.qd:18
  = lines.sum(l => l.margin)

<span class="h">Depends on</span>
  <span class="n">Order.lines</span>  stored
  <span class="n">OrderLine.margin</span>  = (product.price - product.cost) * qty
    <span class="n">OrderLine.product</span>  stored
    <span class="n">OrderLine.qty</span>  stored · default 1
    <span class="n">Product.cost</span>  stored
    <span class="n">Product.price</span>  stored

<span class="h">Stored values underneath, and who writes them</span>
  <span class="n">Product.cost</span>
      <span class="u">user, through forms and API</span></pre>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">History</div>

### A history nobody can rewrite

Every value written to every field is kept, in order, in a log chained by hashes. Editing a table by hand, or altering the history, is detected.

</div>
<div class="q-proof__code">

<pre class="q-term"><span class="c">$ quadra db verify</span>
<span class="ok">✓ hash chain intact, tables match the history (26 records)</span></pre>

</div>
</div>

<div class="q-proof">
<div class="q-proof__text">
<div class="q-proof__n">Preview and revert</div>

### Try it first, take it back after

A change to many records at once is written in Quadra, with a reason and an approval. It runs first as a preview on the real data, checking every rule, and it can be reverted with one command.

</div>
<div class="q-proof__code">

<pre class="q-term"><span class="c">$ quadra change preview raise_prices</span>
raise_prices: Price list 2027: +10%
  approval: a Manager
  <span class="f">~</span> Product#24.list_price: 1.00 EUR → <span class="ok">1.10 EUR</span>
  <span class="f">~</span> Product#25.list_price: 2.50 EUR → <span class="ok">2.75 EUR</span>
<span class="ok">✓ preview: constraints and expectations hold; nothing written</span>

<span class="c">$ quadra change revert raise_prices --approved-by lucia</span></pre>

</div>
</div>

</div>

</div>

<div class="q-section q-page-cta">

## See it for yourself

<p class="q-lead">Break a rule on purpose in the sandbox, and watch Quadra catch it.</p>

<SandboxLink project="welcome" label="Open the Sandbox" />

</div>
