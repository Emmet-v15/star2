// Shared microphone tap: one getUserMedia + AnalyserNode, many readers.
// Browser-dev only; the Tauri engine owns the device, so this never opens
// a second capture there.

let ctx: AudioContext | null = null;
let analyser: AnalyserNode | null = null;
let bins: Uint8Array<ArrayBuffer> | null = null;
let opening: Promise<void> | null = null;

export const inBrowser = typeof window !== "undefined" && !("__TAURI_INTERNALS__" in window);

// Must be called from a user gesture the first time: the AudioContext has to
// be constructed (or resumed) inside one or it stays suspended and reads zeros.
export function ensureMic(): void {
  if (!inBrowser) return;
  if (!ctx) ctx = new AudioContext();
  if (ctx.state === "suspended") void ctx.resume();
  if (opening) return;
  opening = navigator.mediaDevices
    .getUserMedia({ audio: true })
    .then((stream) => {
      if (!ctx) return;
      const an = ctx.createAnalyser();
      an.fftSize = 2048;
      an.smoothingTimeConstant = 0.85;
      ctx.createMediaStreamSource(stream).connect(an);
      analyser = an;
      bins = new Uint8Array(an.frequencyBinCount);
    })
    .catch(() => {
      analyser = null;
    });
}

export function voiceSpectrum(): { bins: Uint8Array<ArrayBuffer>; binHz: number } | null {
  if (!analyser || !bins || !ctx) return null;
  analyser.getByteFrequencyData(bins);
  return { bins, binHz: ctx.sampleRate / analyser.fftSize };
}
