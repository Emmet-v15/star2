<script lang="ts">
  import { call, copyRoom, join, leave, listenEngine, refreshDevices } from "./lib/engine.svelte";
  import { inBrowser, mockAddPeer, mockRemovePeer } from "./lib/browser-dev.svelte";
  import VoiceBars from "./lib/VoiceBars.svelte";

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
        case "peer_joined":
          if (!call.members.some((m) => m.session === e.session)) {
            call.members.push({ session: e.session, name: e.name, self: false, db: null });
          }
          break;
        case "peer_left":
          call.members = call.members.filter((m) => m.session !== e.session);
          break;
        case "peer_level": {
          const m = call.members.find((x) => x.session === e.session);
          if (m) m.db = e.db;
          break;
        }
        case "direct":
          call.status = `connected · ${e.peer}`;
          call.phase = "live";
          break;
        case "status":
          if (/^idle/.test(e.text)) {
            call.fatal = "";
            call.phase = "idle";
            call.members = [];
          }
          call.status = e.text;
          break;
        case "ended":
          call.fatal = e.why;
          call.phase = "down";
          call.members = [];
          break;
        case "stats": {
          const me = call.members.find((m) => m.self);
          if (me) me.db = e.mic_db;
          break;
        }
        default:
          break; // the rest of stats and log stay out of the window; star2.log holds history (CLAUDE.md §4)
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

  const gridCols = $derived(
    call.members.length <= 1
      ? "grid-cols-1"
      : call.members.length <= 4
        ? "grid-cols-2"
        : "grid-cols-3",
  );

  async function copy(): Promise<void> {
    await copyRoom();
    copied = true;
    setTimeout(() => (copied = false), 1100);
  }
</script>

<div class="flex h-screen flex-col font-mono text-[13px] text-neutral-200">
  <header class="flex shrink-0 items-center gap-2 border-b border-line bg-panel px-2.5 py-2">
    <span class="font-bold tracking-widest text-accent">STAR2</span>
    {#if call.phase === "idle" || call.phase === "down"}
      <input
        class="field min-w-0 flex-1"
        placeholder="room token (blank = new room)"
        spellcheck="false"
        autocomplete="off"
        bind:value={roomInput}
        onkeydown={(e) => e.key === "Enter" && join(roomInput)}
      />
      <button class="btn" onclick={() => void join(roomInput)}>Join</button>
    {:else}
      <button class="btn" onclick={() => void leave()}>Leave</button>
    {/if}
    <span class="ml-auto flex shrink-0 items-center gap-1.5 text-dim">
      <span class="size-2 rounded-full {dot}"></span>
      <span class="max-w-[220px] truncate">{call.status}</span>
    </span>
  </header>

  <main class="flex min-h-0 flex-1 flex-col gap-2.5 p-2.5">
    {#if call.phase === "idle" || call.phase === "down"}
      <section class="flex shrink-0 flex-col gap-1.5 rounded-lg border border-line bg-panel p-2.5">
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
      <p class="select-text whitespace-pre-wrap text-bad">{call.fatal}</p>
    {/if}

    <section class="relative flex min-h-0 flex-1 flex-col rounded-lg border border-line bg-panel">
      {#if call.phase === "live" && call.members.length > 0}
        <div class="grid min-h-0 flex-1 gap-2 {gridCols}">
          {#each call.members as m (m.session)}
            <div class="tile">
              <span class="grid size-11 place-items-center rounded-full bg-[#1b1e22] text-base font-bold text-accent">
                {m.name[0]?.toUpperCase()}
              </span>
              <div class="w-full px-4">
                <VoiceBars member={m} />
              </div>
              <span class="flex items-center gap-1.5">
                <span class="truncate">{m.name}</span>
                {#if m.self}
                  <span class="text-dim">(you)</span>
                {/if}
              </span>
            </div>
          {/each}
        </div>
        <button
          class="absolute bottom-2 left-2 max-w-[60%] cursor-pointer truncate rounded border border-line bg-bg/80 px-2 py-0.5 text-[11px] text-dim hover:border-accent hover:text-neutral-200"
          title="room token — click to copy"
          onclick={() => void copy()}
        >
          {copied ? "copied" : call.room}
        </button>
        {#if inBrowser}
          <div class="absolute right-2 bottom-2 flex gap-1.5">
            <button class="btn px-2 py-0.5 text-[11px]" onclick={() => mockAddPeer()}>+ peer</button>
            <button class="btn px-2 py-0.5 text-[11px]" onclick={() => mockRemovePeer()}>− peer</button>
          </div>
        {/if}
      {:else if call.phase === "connecting"}
        <div class="grid h-full place-items-center">
          <div class="flex flex-col items-center gap-2 text-dim">
            <span class="size-2.5 animate-pulse rounded-full bg-warn"></span>
            <span class="text-[11px] tracking-widest uppercase">{call.status || "punching"}</span>
          </div>
        </div>
      {:else}
        <div class="grid h-full place-items-center">
          <div class="flex flex-col items-center gap-1 text-dim">
            <span class="text-2xl text-line">◉</span>
            <span class="text-[11px]">join a room to talk</span>
          </div>
        </div>
      {/if}
    </section>
  </main>
</div>
