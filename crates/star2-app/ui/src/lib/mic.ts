// Shared microphone tap: one getUserMedia + split into per-channel analysers,
// many readers. Browser-dev only; the Tauri engine owns the device, so this
// never opens a second capture there.

let ctx: AudioContext | null = null;
let analyserL: AnalyserNode | null = null;
let analyserR: AnalyserNode | null = null;
let binsL: Uint8Array<ArrayBuffer> | null = null;
let binsR: Uint8Array<ArrayBuffer> | null = null;
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
      const settings = stream.getAudioTracks()[0]?.getSettings();
      const stereo = (settings?.channelCount ?? 1) >= 2;

      const make = (): { a: AnalyserNode; bins: Uint8Array<ArrayBuffer> } => {
        const a = ctx!.createAnalyser();
        a.fftSize = 2048;
        a.smoothingTimeConstant = 0.85;
        return { a, bins: new Uint8Array(a.frequencyBinCount) };
      };

      const L = make();
      const R = make();
      const src = ctx.createMediaStreamSource(stream);
      if (stereo) {
        const splitter = ctx.createChannelSplitter(2);
        src.connect(splitter);
        splitter.connect(L.a, 0);
        splitter.connect(R.a, 1);
      } else {
        src.connect(L.a);
        src.connect(R.a);
      }
      analyserL = L.a;
      binsL = L.bins;
      analyserR = R.a;
      binsR = R.bins;
    })
    .catch(() => {
      analyserL = null;
      analyserR = null;
    });
}

export function voiceSpectrum(channel: 0 | 1): { bins: Uint8Array<ArrayBuffer>; binHz: number } | null {
  const an = channel === 0 ? analyserL : analyserR;
  const buf = channel === 0 ? binsL : binsR;
  if (!an || !buf || !ctx) return null;
  an.getByteFrequencyData(buf);
  return { bins: buf, binHz: ctx.sampleRate / an.fftSize };
}
