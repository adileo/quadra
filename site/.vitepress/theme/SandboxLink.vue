<script setup lang="ts">
import { withBase } from "vitepress";
const props = defineProps<{ project: string; file?: string; label?: string }>();
const href = withBase(`/sandbox?project=${encodeURIComponent(props.project)}${props.file ? `&file=${encodeURIComponent(props.file)}` : ""}`);
</script>

<template>
  <a class="q-sandbox-link" :href="href">
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
      <path d="M5 3.5 12 8l-7 4.5z" />
    </svg>
    {{ label ?? "Open in Sandbox" }}
    <span class="q-sandbox-link__hint">runs in your browser</span>
  </a>
</template>
