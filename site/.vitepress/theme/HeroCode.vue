<script setup lang="ts">
// The hero's "editor window": a short program, colored like the editor does.
// It compiles as is (two entities, two checks, a flow): keep it that way.
</script>

<template>
  <div class="q-hero-code" aria-label="Quadra code example">
    <div class="q-hero-code__bar">
      <span class="dot" /><span class="dot" /><span class="dot" />
      <span class="q-hero-code__file">shop.qd</span>
    </div>
    <pre><code><span class="k">entity</span> <span class="t">Customer</span>:
    name: <span class="t">Text</span>
    credit_limit: <span class="t">Money</span>&lt;<span class="t">EUR</span>&gt; <span class="k">default</span> <span class="n">1_000.00 EUR</span>

<span class="k">entity</span> <span class="t">Order</span>:
    customer: <span class="t">Customer</span>
    amount: <span class="t">Money</span>&lt;<span class="t">EUR</span>&gt;
    state: <span class="k">enum</span>(Draft, Confirmed, Paid) <span class="k">default</span> Draft

    <span class="k">check</span> amount <span class="o">&gt;</span> <span class="n">0</span>, <span class="s">"The amount must be positive"</span>
    <span class="k">check</span> amount <span class="o">&lt;=</span> customer.credit_limit, <span class="s">"Over the credit limit"</span>

    <span class="k">flow</span> state:
        Draft <span class="o">-&gt;</span> Confirmed <span class="k">by</span> confirm
        Confirmed <span class="o">-&gt;</span> Paid <span class="k">by</span> pay</code></pre>
  </div>
</template>
