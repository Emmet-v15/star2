// The browser's microphone: one getUserMedia and one 48 kHz AudioContext,
// shared by the Opus encoder and the visualiser. Opening the device twice
// makes the two captures fight over it, and on Windows the second one loses.
// A Tauri build never gets here - the engine owns the device there.

let ctx: AudioContext | null = null;
let stream: MediaStream | null = null;
let source: MediaStreamAudioSourceNode | null = null;
let analyserL: AnalyserNode | null = null;
let analyserR: AnalyserNode | null = null;
let binsL: Uint8Array<ArrayBuffer> | null = null;
let binsR: Uint8Array<ArrayBuffer> | null = null;
let openFor: string | undefined;
let opening: Promise<void> | null = null;

export const inBrowser = typeof window !== "undefined" && !("__TAURI_INTERNALS__" in window);

export type MicGraph = { ctx: AudioContext; source: MediaStreamAudioSourceNode };

export function micGraph(): MicGraph | null {
  return ctx && source ? { ctx, source } : null;
}

// Must be reached from a user gesture the first time: the AudioContext has to
// be constructed inside one or it stays suspended and reads zeros. Called with
// no id it means "something, anything" and leaves an open device alone.
export async function ensureMic(deviceId?: string): Promise<void> {
  if (!inBrowser) return;
  if (!ctx) ctx = new AudioContext({ sampleRate: 48000 });
  if (ctx.state === "suspended") await ctx.resume();
  if (stream && (deviceId === undefined || deviceId === openFor)) return;
  if (opening && deviceId === undefined) return opening;
  if (stream) closeStream();

  openFor = deviceId;
  opening = navigator.mediaDevices
    .getUserMedia({
      // The visualiser wants raw channels: Firefox defaults the mic to mono
      // unless channelCount is asked for, and echo/gain processing downmixes
      // to mono on some platforms, so all of it is off.
      audio: {
        ...(deviceId ? { deviceId: { exact: deviceId } } : {}),
        channelCount: 2,
        echoCancellation: false,
        noiseSuppression: false,
        autoGainControl: false,
      },
    })
    .then((s) => {
      if (!ctx) return;
      stream = s;
      const stereo = (s.getAudioTracks()[0]?.getSettings().channelCount ?? 1) >= 2;

      const make = (): { a: AnalyserNode; bins: Uint8Array<ArrayBuffer> } => {
        const a = ctx!.createAnalyser();
        a.fftSize = 2048;
        a.smoothingTimeConstant = 0.85;
        return { a, bins: new Uint8Array(a.frequencyBinCount) };
      };

      const L = make();
      const R = make();
      source = ctx.createMediaStreamSource(s);
      if (stereo) {
        const splitter = ctx.createChannelSplitter(2);
        source.connect(splitter);
        splitter.connect(L.a, 0);
        splitter.connect(R.a, 1);
      } else {
        source.connect(L.a);
        source.connect(R.a);
      }
      analyserL = L.a;
      binsL = L.bins;
      analyserR = R.a;
      binsR = R.bins;
    })
    .finally(() => {
      opening = null;
    });

  return opening;
}

function closeStream(): void {
  stream?.getTracks().forEach((t) => t.stop());
  stream = null;
  source = null;
  analyserL = null;
  analyserR = null;
  binsL = null;
  binsR = null;
}

// Releases the device so the browser stops showing a recording indicator while
// the window sits idle.
export function closeMic(): void {
  closeStream();
  openFor = undefined;
}

// Routes playback at a chosen output. Chrome only; elsewhere the default device
// is the only device, which is worth less than a broken call.
export async function setSink(deviceId?: string): Promise<void> {
  const sinkable = ctx as (AudioContext & { setSinkId?: (id: string) => Promise<void> }) | null;
  if (!deviceId || !sinkable?.setSinkId) return;
  try {
    await sinkable.setSinkId(deviceId);
  } catch (_) {}
}

export function voiceSpectrum(channel: 0 | 1): { bins: Uint8Array<ArrayBuffer>; binHz: number } | null {
  const an = channel === 0 ? analyserL : analyserR;
  const buf = channel === 0 ? binsL : binsR;
  if (!an || !buf || !ctx) return null;
  an.getByteFrequencyData(buf);
  return { bins: buf, binHz: ctx.sampleRate / an.fftSize };
}
