<script setup lang="ts">
// A code block shown by its first lines, with a fade and "Show all": long code doesn't land on
// the reader at once. Blocks no longer than `lines` are shown whole, with no button.
import { onMounted, ref } from "vue";

const props = withDefaults(defineProps<{ lines?: number }>(), { lines: 12 });
const root = ref<HTMLElement | null>(null);
const total = ref(0);
const open = ref(false);
const height = ref<string | undefined>();

onMounted(() => {
  const ls = root.value?.querySelectorAll("pre .line");
  if (!ls || ls.length <= props.lines) return;
  total.value = ls.length;
  // the block's frame is positioned: a line's offsetTop is measured from the block's top
  const cut = (ls[props.lines] as HTMLElement).offsetTop;
  height.value = `${cut + 8}px`;
});
</script>

<template>
  <div ref="root" class="q-fold" :class="{ folded: total && !open }">
    <div class="q-fold__body" :style="total && !open ? { maxHeight: height } : undefined"><slot /></div>
    <button v-if="total && !open" class="q-fold__more" @click="open = true">Show all {{ total }} lines</button>
  </div>
</template>

<style scoped>
.q-fold { position: relative; margin-bottom: 12px; }
.q-fold__body { overflow: hidden; border-radius: var(--q-radius, 8px); }
.q-fold__body :deep(div[class*="language-"]) { margin: 0 !important; }
.q-fold.folded .q-fold__body::after { content: ""; position: absolute; left: 0; right: 0; bottom: 0; height: 96px; border-radius: 0 0 var(--q-radius, 8px) var(--q-radius, 8px); background: linear-gradient(to bottom, transparent, var(--vp-code-block-bg) 85%); pointer-events: none; }
.q-fold__more { position: absolute; left: 50%; bottom: 14px; z-index: 1; transform: translateX(-50%); padding: 6px 14px; border-radius: 6px; background: var(--vp-c-bg); color: var(--vp-c-text-1); font-size: 13px; font-weight: 500; white-space: nowrap; transition: background-color 0.15s; }
.q-fold__more:hover { background: var(--q-hl, #fde047); color: #171717; }
</style>
