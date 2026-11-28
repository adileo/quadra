<script setup lang="ts">
// A line to paste into an AI agent: it points the agent at llms.txt, the docs written for it.
import { ref } from "vue";

const prompt = "Read https://quadra.run/llms.txt, then help me build my app with Quadra.";
const copied = ref(false);

async function copy() {
  let ok = false;
  try {
    await navigator.clipboard.writeText(prompt);
    ok = true;
  } catch {
    // no async clipboard (permissions, older browsers): copy through a hidden selection
    const area = document.createElement("textarea");
    area.value = prompt;
    area.style.cssText = "position:fixed;opacity:0";
    document.body.appendChild(area);
    area.select();
    ok = document.execCommand("copy");
    area.remove();
  }
  if (!ok) return;
  copied.value = true;
  setTimeout(() => (copied.value = false), 1600);
}
</script>

<template>
  <div class="q-agent">
    <div class="q-agent__label">Using an AI agent? Paste this into it</div>
    <div class="q-agent__box">
      <code>{{ prompt }}</code>
      <button type="button" @click="copy">{{ copied ? "Copied" : "Copy" }}</button>
    </div>
  </div>
</template>

<style scoped>
.q-agent { margin-top: 36px; max-width: 640px; }
.q-agent__label { font-size: 14px; color: var(--vp-c-text-2); margin-bottom: 8px; }
.q-agent__box { display: flex; align-items: center; gap: 12px; padding: 10px 10px 10px 14px; border-radius: var(--q-radius); background: var(--q-panel); }
.q-agent__box code { flex: 1; min-width: 0; font-family: var(--vp-font-family-mono); font-size: 13px; line-height: 1.5; background: none; padding: 0; color: var(--vp-c-text-1); user-select: all; }
.q-agent__box button { flex: none; font: 500 13px var(--vp-font-family-base); padding: 5px 10px; border-radius: 6px; background: var(--q-hl); color: #171717; cursor: pointer; }
.q-agent__box button:hover { background: var(--q-hl-hover); }
</style>
