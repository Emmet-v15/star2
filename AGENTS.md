# AGENTS.md — frontend rules (`crates/star2-app/ui`)

Same status as `CLAUDE.md`: hard constraints, not preferences. When a change
conflicts with one of them, the change is wrong.

## Svelte 5, runes mode only. Never `$:`.

`svelte.config.js` sets `compilerOptions: { runes: true }`, so legacy syntax is
a compile error, not a style opinion. Concretely:

- No `$:` — values are `$derived(...)`, side effects are `$effect(...)`.
- No `export let` — props come from `$props()`.
- No `svelte/store` (`writable`, `readable`, `derived`, `get`) — shared state is
  `$state` in a `.svelte.ts` module.
- No `on:click` / `on:change` — event attributes are lowercase: `onclick`,
  `onchange`.
- No `<slot>` — pass and render snippets.
- Subscriptions (Tauri `listen`) live inside `$effect` and return their cleanup.

## TypeScript

Strict. The backend contract types live in `src/lib/engine.svelte.ts` and mirror
the Rust commands in `crates/star2-app/src/main.rs` — when Rust changes, change
them in the same commit. Do not invent commands that `main.rs` does not register.

## Scope of the window

`CLAUDE.md` §4 is the spec: room token, join, status, two device pickers. A new
control must earn its place the way the device pickers did. Diagnostics go to
`star2.log` beside the binary, never to a pane.
