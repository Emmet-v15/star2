import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";

export default {
  preprocess: vitePreprocess(),
  // Runes mode is law (AGENTS.md). This turns any legacy reactive syntax
  // (`$:`, `export let`, stores) into a compile error instead of a review note.
  compilerOptions: { runes: true },
};
