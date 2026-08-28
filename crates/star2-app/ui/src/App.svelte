<script lang="ts">
  import { call, copyRoom, join, leave, listenEngine, refreshDevices } from "./lib/engine.svelte";

  let roomInput = $state(call.room);
  let copied = $state(false);

  $effect(() => {
    void refreshDevices();
    const un = listenEngine((e) => {
      switch (e.t) {
        case "room_joined":
          call.room = e.room;
          localStorage.setItem("star2.room", e.room);
          if (call.phase !== "live") call.phase = "connecting";
          break;
        case "direct":
          call.status = `connected · ${e.peer}`;
          call.phase = "live";
          break;
        case "status":
          if (/^idle/.test(e.text)) {
            call.fatal = "";
            call.phase = "idle";
          }
          call.status = e.text;
          break;
        case "ended":
          call.fatal = e.why;
          call.phase = "down";
          break;
        default:
          break; // stats and log stay out of the window; star2.log holds history (CLAUDE.md §4)
      }
    });
    return () => {
      void un.then((f) => f());
    };
  });

  const dot = $derived(
    call.phase === "live"
      ? "bg-ok shadow-[0_0_6px_var(--color-ok)]"
      : call.phase === "connecting"
        ? "bg-warn"
        : call.phase === "down"
          ? "bg-bad"
          : "bg-dim",
  );

  async function copy(): Promise<void> {
    await copyRoom();
    copied = true;
    setTimeout(() => (copied = false), 1100);
  }
</script>

<div class="flex h-screen flex-col font-mono text-[13px] text-neutral-200">
  <header class="flex items-center gap-2 border-b border-line bg-panel px-2.5 py-2">
    <span class="font-bold tracking-widest text-accent">STAR2</span>
    {#if call.phase === "idle" || call.phase === "down"}
      <span class="flex min-w-0 flex-1 gap-2">
        <input
          class="field min-w-0 flex-1"
          placeholder="room token (blank = new room)"
          spellcheck="false"
          autocomplete="off"
          bind:value={roomInput}
          onkeydown={(e) => e.key === "Enter" && join(roomInput)}
        />
        <button class="btn" onclick={() => void join(roomInput)}>Join</button>
      </span>
    {:else}
      <span class="flex min-w-0 flex-1 items-center gap-2">
        <button class="btn" onclick={() => void leave()}>Leave</button>
        <input
          readonly
          class="field min-w-0 flex-1 border-none bg-transparent font-bold tracking-wide text-white"
          title="room token — share it"
          bind:value={call.room}
        />
        <button class="btn" onclick={() => void copy()}>{copied ? "Copied" : "Copy"}</button>
      </span>
    {/if}
    <span class="ml-auto flex shrink-0 items-center gap-1.5 text-dim">
      <span class="size-2 rounded-full {dot}"></span>
      <span class="truncate">{call.status}</span>
    </span>
  </header>

  {#if call.phase === "idle" || call.phase === "down"}
    <section class="m-2.5 flex flex-col gap-1.5 rounded-lg border border-line bg-panel p-2.5">
      <div class="text-[11px] tracking-wide text-dim">AUDIO</div>
      <label class="grid grid-cols-[34px_1fr] items-center gap-2">
        <span class="text-[11px] tracking-wide text-dim">IN</span>
        <select
          class="field"
          bind:value={call.input}
          onchange={() => localStorage.setItem("star2.in", call.input)}
        >
          {#each call.devices.input as d (d.name)}
            <option value={d.name}>{d.name}{d.default ? "  (default)" : ""}</option>
          {/each}
        </select>
      </label>
      <label class="grid grid-cols-[34px_1fr] items-center gap-2">
        <span class="text-[11px] tracking-wide text-dim">OUT</span>
        <select
          class="field"
          bind:value={call.output}
          onchange={() => localStorage.setItem("star2.out", call.output)}
        >
          {#each call.devices.output as d (d.name)}
            <option value={d.name}>{d.name}{d.default ? "  (default)" : ""}</option>
          {/each}
        </select>
      </label>
    </section>
  {/if}

  {#if call.fatal}
    <p class="mx-2.5 mb-2.5 select-text whitespace-pre-wrap text-bad">{call.fatal}</p>
  {/if}
</div>
