<script lang="ts">
  import { type Member } from "./engine.svelte";
  import { ensureMic, voiceWave } from "./mic";

  const POINTS = 96;
  const DB_FLOOR = -60;
  const DB_SPAN = 45;

  let { member }: { member: Member } = $props();

  let canvas: HTMLCanvasElement;
  const modes = [1, 2, 3].map((n, i) => ({
    n,
    speed: 1.7 + i * 1.3,
    phase: Math.random() * Math.PI * 2,
    mix: [0.6, 0.3, 0.15][i] ?? 0,
  }));

  // Self traces the real oscilloscope waveform; a remote peer gets a standing
  // wave (fixed string modes) scaled by the level the engine reports for them.
  function displacement(x: number, t: number): number {
    if (member.self) {
      const wave = voiceWave();
      if (wave) {
        const j = Math.floor(x * (wave.length - 1));
        const s = wave[j] ?? 0;
        return Math.tanh(s * 14) * 0.9 + Math.tanh(s * 40) * 0.1;
      }
      return 0;
    }
    const db = member.db ?? DB_FLOOR;
    const amp = Math.min(1, Math.max(0, (db - DB_FLOOR) / DB_SPAN));
    let v = 0;
    for (const m of modes) {
      v += m.mix * Math.sin(Math.PI * m.n * x) * Math.sin(2 * Math.PI * m.speed * t + m.phase);
    }
    return v * amp;
  }

  function draw(): void {
    const raf = requestAnimationFrame(draw);
    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth;
    const h = canvas.clientHeight;
    if (w === 0 || h === 0) return;
    if (canvas.width !== w * dpr || canvas.height !== h * dpr) {
      canvas.width = w * dpr;
      canvas.height = h * dpr;
    }
    const g = canvas.getContext("2d");
    if (!g) return;
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, w, h);

    const t = performance.now() / 1000;
    const mid = h / 2;
    const reach = h / 2 - 2;

    const trace = (scale: number, alpha: number, width: number, glow: number): void => {
      g.beginPath();
      for (let i = 0; i < POINTS; i++) {
        const x = (i / (POINTS - 1)) * w;
        const y = mid + displacement(i / (POINTS - 1), t) * reach * scale;
        if (i === 0) g.moveTo(x, y);
        else g.lineTo(x, y);
      }
      g.strokeStyle = `rgba(62,166,255,${alpha})`;
      g.lineWidth = width;
      g.shadowColor = "rgba(62,166,255,0.8)";
      g.shadowBlur = glow;
      g.lineJoin = "round";
      g.stroke();
      g.shadowBlur = 0;
    };

    trace(0.55, 0.25, 1, 0);
    trace(1, 0.95, 1.8, 6);
  }

  $effect(() => {
    ensureMic();
    const raf = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(raf);
  });
</script>

<canvas bind:this={canvas} class="block h-6 w-full" aria-hidden="true"></canvas>
