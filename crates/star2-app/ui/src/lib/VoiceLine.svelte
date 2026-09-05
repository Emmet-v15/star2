<script lang="ts">
  import { type Member } from "./engine.svelte";
  import { ensureMic, voiceSpectrum } from "./mic";

  const POINTS = 64;
  const F_MIN = 60;
  const F_MAX = 16000;
  const DB_FLOOR = -60;
  const DB_SPAN = 45;

  let { member }: { member: Member } = $props();

  let canvas: HTMLCanvasElement;
  const smooth = new Float32Array(POINTS);
  const phases = Array.from({ length: POINTS }, () => Math.random() * Math.PI * 2);

  // Self traces the real FFT; a remote peer gets a voice-formant shape driven
  // by the level the engine reports for them.
  function target(i: number, t: number): number {
    if (member.self) {
      const mic = voiceSpectrum();
      if (mic) {
        const fLo = F_MIN * Math.pow(F_MAX / F_MIN, i / POINTS);
        const fHi = F_MIN * Math.pow(F_MAX / F_MIN, (i + 1) / POINTS);
        const lo = Math.max(1, Math.floor(fLo / mic.binHz));
        const hi = Math.min(mic.bins.length - 1, Math.max(lo + 1, Math.ceil(fHi / mic.binHz)));
        let sum = 0;
        for (let b = lo; b <= hi; b++) sum += mic.bins[b] ?? 0;
        return Math.sqrt(sum / ((hi - lo + 1) * 255));
      }
      return 0;
    }
    const db = member.db ?? DB_FLOOR;
    const amp = Math.min(1, Math.max(0, (db - DB_FLOOR) / DB_SPAN));
    const fc = Math.log10(F_MIN * Math.pow(F_MAX / F_MIN, (i + 0.5) / POINTS));
    const ph = phases[i] ?? 0;
    const formant =
      Math.exp(-Math.pow((fc - 2.6) / 0.35, 2)) * 1.0 +
      Math.exp(-Math.pow((fc - 3.1) / 0.5, 2)) * 0.55 +
      Math.exp(-Math.pow((fc - 3.9) / 0.7, 2)) * 0.25;
    const breathe =
      0.8 +
      0.2 *
        (Math.sin(t * 1.9 + ph) * 0.5 + Math.sin(t * 3.1 + ph * 1.7) * 0.3 + Math.sin(t * 0.7 + ph * 0.6) * 0.2);
    return Math.min(1, formant * breathe * amp);
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
    for (let i = 0; i < POINTS; i++) {
      const v = target(i, t);
      smooth[i] = (smooth[i] ?? 0) + (v - (smooth[i] ?? 0)) * (v > (smooth[i] ?? 0) ? 0.45 : 0.1);
    }

    const xs = new Float32Array(POINTS);
    const up = new Float32Array(POINTS);
    const down = new Float32Array(POINTS);
    for (let i = 0; i < POINTS; i++) {
      xs[i] = (i / (POINTS - 1)) * w;
      up[i] = h / 2 - (smooth[i] ?? 0) * (h / 2 - 1);
      down[i] = h / 2 + (smooth[i] ?? 0) * (h / 2 - 1);
    }

    const path = new Path2D();
    path.moveTo(xs[0] ?? 0, up[0] ?? 0);
    for (let i = 1; i < POINTS - 1; i++) {
      const mx = ((xs[i] ?? 0) + (xs[i + 1] ?? 0)) / 2;
      const my = ((up[i] ?? 0) + (up[i + 1] ?? 0)) / 2;
      path.quadraticCurveTo(xs[i] ?? 0, up[i] ?? 0, mx, my);
    }
    for (let i = POINTS - 1; i > 0; i--) {
      const mx = ((xs[i] ?? 0) + (xs[i - 1] ?? 0)) / 2;
      const my = ((down[i] ?? 0) + (down[i - 1] ?? 0)) / 2;
      path.quadraticCurveTo(xs[i] ?? 0, down[i] ?? 0, mx, my);
    }
    path.closePath();

    g.fillStyle = "rgba(62,166,255,0.16)";
    g.fill(path);
    g.strokeStyle = "rgba(62,166,255,0.95)";
    g.lineWidth = 1.6;
    g.lineJoin = "round";
    g.shadowColor = "rgba(62,166,255,0.8)";
    g.shadowBlur = 6;
    g.stroke(path);
    g.shadowBlur = 0;

    g.strokeStyle = "rgba(124,130,140,0.35)";
    g.lineWidth = 1;
    g.beginPath();
    g.moveTo(0, h / 2);
    g.lineTo(w, h / 2);
    g.stroke();
  }

  $effect(() => {
    ensureMic();
    const raf = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(raf);
  });
</script>

<canvas bind:this={canvas} class="block h-10 w-full" aria-hidden="true"></canvas>
