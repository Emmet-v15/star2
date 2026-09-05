<script lang="ts">
  import { call } from "./engine.svelte";
  import { inBrowser } from "./browser-dev.svelte";

  const BARS = 64;
  const F_MIN = 60;
  const F_MAX = 16000;
  const DB_FLOOR = -60;
  const DB_SPAN = 45;

  let canvas: HTMLCanvasElement;

  let analyser: AnalyserNode | null = null;
  let bins: Uint8Array<ArrayBuffer> | null = null;
  let binHz = 23.4;

  const peaks = new Float32Array(BARS);
  const phases = Array.from({ length: BARS }, () => Math.random() * Math.PI * 2);

  async function tapRealMic(): Promise<void> {
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      const ac = new AudioContext();
      const an = ac.createAnalyser();
      an.fftSize = 2048;
      an.smoothingTimeConstant = 0.78;
      ac.createMediaStreamSource(stream).connect(an);
      analyser = an;
      bins = new Uint8Array(an.frequencyBinCount);
      binHz = ac.sampleRate / an.fftSize;
    } catch {
      analyser = null;
    }
  }

  function barLevel(i: number, t: number): number {
    if (analyser && bins) {
      const fLo = F_MIN * Math.pow(F_MAX / F_MIN, i / BARS);
      const fHi = F_MIN * Math.pow(F_MAX / F_MIN, (i + 1) / BARS);
      let lo = Math.max(1, Math.floor(fLo / binHz));
      const hi = Math.min(bins.length - 1, Math.max(lo + 1, Math.ceil(fHi / binHz)));
      let sum = 0;
      for (let b = lo; b <= hi; b++) sum += bins[b] ?? 0;
      const v = sum / ((hi - lo + 1) * 255);
      return Math.sqrt(v); // perceptual lift: quiet speech stays visible on a linear scale
    }
    const micDb = call.micDb ?? DB_FLOOR;
    const amp = Math.min(1, Math.max(0, (micDb - DB_FLOOR) / DB_SPAN));
    const fc = F_MIN * Math.pow(F_MAX / F_MIN, (i + 0.5) / BARS);
    const lfc = Math.log10(fc);
    const ph = phases[i] ?? 0;
    const formant =
      Math.exp(-Math.pow((lfc - 2.6) / 0.35, 2)) * 1.0 +
      Math.exp(-Math.pow((lfc - 3.1) / 0.5, 2)) * 0.55 +
      Math.exp(-Math.pow((lfc - 3.9) / 0.7, 2)) * 0.25;
    const flutter =
      0.55 +
      0.45 *
        (Math.sin(t * 7 + ph) * 0.5 +
          Math.sin(t * 13.7 + ph * 2.1) * 0.3 +
          Math.sin(t * 3.1 + ph * 0.7) * 0.2);
    return Math.min(1, formant * flutter * amp);
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
    if (analyser && bins) analyser.getByteFrequencyData(bins);
    const gap = 2;
    const bw = (w - gap * (BARS - 1)) / BARS;
    const mid = h * 0.62;

    g.strokeStyle = "rgba(126,130,140,0.25)";
    g.beginPath();
    g.moveTo(0, mid + 0.5);
    g.lineTo(w, mid + 0.5);
    g.stroke();

    for (let i = 0; i < BARS; i++) {
      const v = barLevel(i, t);
      const bh = Math.max(1.5, v * (h * 0.82));
      const x = i * (bw + gap);
      const y = mid - bh * 0.72;

      g.shadowColor = "rgba(62,166,255,0.8)";
      g.shadowBlur = 6;
      g.fillStyle = v > 0.02 ? "#3ea6ff" : "#23262b";
      g.fillRect(x, y, bw, bh);
      g.shadowBlur = 0;

      peaks[i] = Math.max((peaks[i] ?? 0) * 0.985 - 0.0006, v);
      const py = mid - (peaks[i] ?? 0) * (h * 0.82) * 0.72 - 2;
      g.fillStyle = "rgba(229,229,229,0.7)";
      g.fillRect(x, py, bw, 1);
    }

    g.fillStyle = "rgba(124,130,140,0.6)";
    g.font = "9px ui-monospace, monospace";
    for (const [f, label] of [
      [100, "100"],
      [1000, "1k"],
      [10000, "10k"],
    ] as const) {
      const x = (Math.log10(f) - Math.log10(F_MIN)) / (Math.log10(F_MAX) - Math.log10(F_MIN)) * w;
      g.fillRect(x, h - 4, 1, 3);
      g.fillText(label, Math.min(x + 2, w - 16), h - 6);
    }
  }

  $effect(() => {
    if (inBrowser) void tapRealMic();
    const raf = requestAnimationFrame(draw);
    return () => {
      cancelAnimationFrame(raf);
      if (analyser) {
        void (analyser.context as AudioContext).close();
        analyser = null;
      }
    };
  });
</script>

<canvas bind:this={canvas} class="block h-12 w-full" title="live spectrum — log 60 Hz – 16 kHz"></canvas>
