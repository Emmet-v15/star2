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
  const smooth = new Float32Array(POINTS * 2);
  const phases = Array.from({ length: POINTS }, () => Math.random() * Math.PI * 2);

  // Self traces the real FFT per channel - top curve is the left channel,
  // bottom the right; a mono source feeds both, so the lens stays symmetric.
  // A remote peer has one level, so its two halves are the same shape.
  const HALF = POINTS / 2;

  function target(i: number, t: number, channel: 0 | 1): number {
    const center = (POINTS - 1) / 2;
    const d = Math.abs(i - center) / center;
    const k = Math.min(HALF - 1, Math.round(d * (HALF - 1)));
    if (member.self) {
      const mic = voiceSpectrum(channel);
      if (mic) {
        const fLo = F_MIN * Math.pow(F_MAX / F_MIN, k / HALF);
        const fHi = F_MIN * Math.pow(F_MAX / F_MIN, (k + 1) / HALF);
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
    const fc = Math.log10(F_MIN * Math.pow(F_MAX / F_MIN, (k + 0.5) / HALF));
    const ph = phases[i] ?? 0;
    const formant =
      Math.exp(-Math.pow((fc - 2.15) / 0.5, 2)) * 1.0 +
      Math.exp(-Math.pow((fc - 2.75) / 0.55, 2)) * 0.55 +
      Math.exp(-Math.pow((fc - 3.4) / 0.75, 2)) * 0.3;
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
      for (const [ch, off] of [
        [0, 0],
        [1, POINTS],
      ] as const) {
        const v = target(i, t, ch);
        const s = smooth[off + i] ?? 0;
        smooth[off + i] = s + (v - s) * (v > s ? 0.45 : 0.1);
      }
    }

    const xs = new Float32Array(POINTS);
    const up = new Float32Array(POINTS);
    const down = new Float32Array(POINTS);
    for (let i = 0; i < POINTS; i++) {
      xs[i] = (i / (POINTS - 1)) * w;
      up[i] = h / 2 - (smooth[i] ?? 0) * (h / 2 - 1);
      down[i] = h / 2 + (smooth[POINTS + i] ?? 0) * (h / 2 - 1);
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

<canvas
  bind:this={canvas}
  class="block h-10 w-full"
  title="spectrum — top: left channel, bottom: right"
  aria-hidden="true"
></canvas>
