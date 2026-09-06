// The browser's engine. It answers the same commands main.rs registers and
// emits the same events, so App.svelte cannot tell a web build from a Tauri
// one - that is the point, and why there is no second UI.
//
// Signalling rides the rendezvous WebSocket the native client uses. Media rides
// an unreliable, unordered WebRTC data channel carrying exactly the datagram a
// native peer would have put on UDP (star-proto's MediaHeader):
//
//   byte 0      version (1)
//   byte 1      flags (0 - mono, no RED)
//   bytes 2-6   session id, LE u32
//   bytes 6-8   sequence, LE u16
//   bytes 8-12  timestamp, LE u32, in 48 kHz samples
//   rest        one 5 ms Opus frame (WebCodecs AudioEncoder)
//
// The native side answers the SDP offer with str0m (ICE-lite) and feeds the
// channel into its ordinary jitter buffer and playout.

import type { AudioDevice, AudioDevices, EngineEvent } from "./engine.svelte";
import { closeMic, ensureMic, micGraph, setSink } from "./mic";
import { isRoomToken, newRoomToken } from "./room";

const PROTO_VERSION = 1;
const FRAME_SAMPLES = 240;
const SAMPLE_RATE = 48000;
const BITRATE = 128000;
const LEVEL_EVERY = 120;
const NAME = "guest-web";

const params = new URLSearchParams(location.hash.slice(1));
const RENDEZVOUS =
  params.get("rv") ??
  (location.protocol === "https:" ? "wss://star.v15.studio/star2" : "ws://localhost:9101");
// The same token star2.exe ships compiled in. It admits a client to the
// rendezvous server and gates nothing else.
const TOKEN = params.get("token") ?? "ad7afaabdfe6a6636c3e3e478321039c";

type Listener = (e: EngineEvent) => void;
const listeners = new Set<Listener>();
const emit = (e: EngineEvent): void => listeners.forEach((l) => l(e));
const status = (text: string): void => emit({ t: "status", text });

export function listenWeb(l: Listener): () => void {
  listeners.add(l);
  return () => listeners.delete(l);
}

const deviceIds = new Map<string, string>();

let ws: WebSocket | null = null;
let session = 0;
let peer = 0;
let pc: RTCPeerConnection | null = null;
let dc: RTCDataChannel | null = null;
let encoder: AudioEncoder | null = null;
let decoder: AudioDecoder | null = null;
let capture: AudioWorkletNode | null = null;
let play: AudioWorkletNode | null = null;
let workletReady = false;
let levelTimer: ReturnType<typeof setInterval> | undefined;
let seq = 0;
let ts = 0;
let tsUs = 0;
let rxTsUs = 0;
let rxSum = 0;
let rxCount = 0;
let hostCands = 0;
let explained = false;
let welcomed = false;

async function audioDevices(): Promise<AudioDevices> {
  const all = await navigator.mediaDevices.enumerateDevices();
  deviceIds.clear();
  // A device name has to be unique: it is the key of the picker's list and the
  // string remembered in localStorage. Browsers do not oblige - labels are
  // empty until the mic has been granted once, and Firefox hands out the same
  // label for several entries - so number the collisions.
  const list = (kind: MediaDeviceKind, fallback: string): AudioDevice[] => {
    const seen = new Map<string, number>();
    return all
      .filter((d) => d.kind === kind)
      .map((d, i) => {
        const label = d.label || `${fallback} ${i + 1}`;
        const nth = (seen.get(label) ?? 0) + 1;
        seen.set(label, nth);
        const name = nth === 1 ? label : `${label} (${nth})`;
        deviceIds.set(`${kind}:${name}`, d.deviceId);
        return { name, default: d.deviceId === "default" };
      });
  };
  return { input: list("audioinput", "Microphone"), output: list("audiooutput", "Speakers") };
}

function sendMsg(msg: Record<string, unknown>): void {
  if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify(msg));
}

async function join(room: string, input: string, output: string): Promise<string> {
  await ensureMic(deviceIds.get(`audioinput:${input}`));
  if (!micGraph()) throw new Error("microphone refused");
  await setSink(deviceIds.get(`audiooutput:${output}`));

  const token = isRoomToken(room) ? room : newRoomToken(room);
  explained = false;
  welcomed = false;
  status("connecting to rendezvous");

  await new Promise<void>((resolve, reject) => {
    const sock = new WebSocket(RENDEZVOUS);
    ws = sock;
    sock.onopen = () =>
      sendMsg({ t: "Hello", name: NAME, ver: PROTO_VERSION, token: TOKEN, build: "guest-web" });
    sock.onmessage = (e) => {
      let m: Record<string, unknown>;
      try {
        m = JSON.parse(String(e.data));
      } catch {
        return;
      }
      if (m["t"] === "Welcome") {
        session = Number(m["session"]);
        welcomed = true;
        sendMsg({ t: "Join", room: token });
        status("waiting for a peer");
        resolve();
        return;
      }
      if (m["t"] === "Error") {
        explained = true;
        const why = String(m["msg"]);
        if (welcomed) emit({ t: "ended", why });
        else reject(new Error(why));
        return;
      }
      handleServer(m);
    };
    sock.onclose = () => {
      // Only guess when nothing already said why: a refusal or a press of Leave
      // has the real reason on screen, and "unreachable" would bury it.
      if (!welcomed) {
        if (!explained) reject(new Error("rendezvous unreachable"));
        return;
      }
      // CLAUDE.md §4: a call in progress outlives the rendezvous server. The
      // media is already peer-to-peer, so only a call still being set up dies
      // with the socket.
      if (!pc && !explained) emit({ t: "ended", why: "rendezvous closed" });
    };
  });

  return token;
}

function handleServer(m: Record<string, unknown>): void {
  switch (m["t"]) {
    case "Room": {
      const members = (m["members"] ?? []) as { session: number; name: string }[];
      const other = members.find((x) => x.session !== session);
      if (other) meet(other.session, other.name);
      break;
    }
    case "Joined":
      meet(Number(m["session"]), String(m["name"] ?? "peer"));
      break;
    case "SdpAnswer":
      if (pc && Number(m["from"]) === peer) {
        void pc.setRemoteDescription({ type: "answer", sdp: String(m["sdp"]) });
      }
      break;
    case "SdpOffer":
      // meet() left us answering: we have a peer but no connection yet.
      if (!pc && Number(m["from"]) === peer) void accept(peer, String(m["sdp"]));
      break;
    case "Left":
      if (Number(m["session"]) === peer) hangup("peer left", false);
      break;
    default:
      break;
  }
}

function meet(who: number, name: string): void {
  if (pc) return;
  peer = who;
  emit({ t: "peer_joined", session: who, name });
  // Two guests would both offer and both wait forever. Between guests the
  // lower session id offers and the other waits to answer. A native peer
  // never offers at all, so a guest always offers to one.
  if (name === NAME && who < session) return;
  void invite(who);
}

const RTC_CONFIG: RTCConfiguration = {
  iceServers: [{ urls: ["stun:stun.l.google.com:19302", "stun:stun1.l.google.com:19302"] }],
};

function wireChannel(ch: RTCDataChannel): void {
  dc = ch;
  ch.binaryType = "arraybuffer";
  ch.onopen = () => void startAudio();
  // When ICE has already failed, the connection handler owns the hangup and
  // its diagnosis; a channel is only "closed" news on a healthy connection.
  ch.onclose = () => {
    if (pc?.connectionState !== "failed") hangup("channel closed", false);
  };
  ch.onmessage = (e) => onChannelData(e.data as ArrayBuffer);
}

function watchPc(conn: RTCPeerConnection): void {
  conn.onconnectionstatechange = () => {
    if (conn.connectionState === "failed") hangup("connection failed", true);
  };
}

// Count what we are actually signalling: the candidates a peer could dial.
// WebRTC leak protection (Firefox's media.peerconnection.ice.no_host) leaves
// the srflx addresses only, and then nobody on our own network can reach us.
function gatherCounter(conn: RTCPeerConnection): void {
  conn.onicecandidate = (e) => {
    if (e.candidate?.candidate.includes("typ host")) hostCands++;
  };
}

async function invite(to: number): Promise<void> {
  status("negotiating");
  pc = new RTCPeerConnection(RTC_CONFIG);
  wireChannel(pc.createDataChannel("media", { ordered: false, maxRetransmits: 0 }));
  watchPc(pc);
  gatherCounter(pc);

  await pc.setLocalDescription(await pc.createOffer());
  await gathered(pc);
  sendMsg({ t: "SdpOffer", to, sdp: pc.localDescription?.sdp ?? "" });
  status("offer sent, waiting for answer");
}

async function accept(to: number, sdp: string): Promise<void> {
  status("answering");
  pc = new RTCPeerConnection(RTC_CONFIG);
  pc.ondatachannel = (e) => wireChannel(e.channel);
  watchPc(pc);
  gatherCounter(pc);

  await pc.setRemoteDescription({ type: "offer", sdp });
  await pc.setLocalDescription(await pc.createAnswer());
  await gathered(pc);
  sendMsg({ t: "SdpAnswer", to, sdp: pc.localDescription?.sdp ?? "" });
}

function gathered(conn: RTCPeerConnection): Promise<void> {
  if (conn.iceGatheringState === "complete") return Promise.resolve();
  return new Promise((res) => {
    conn.addEventListener("icegatheringstatechange", () => {
      if (conn.iceGatheringState === "complete") res();
    });
    setTimeout(res, 2000); // never wait forever on a stalled gather
  });
}

// The window says "connected · <addr>", so read the address actually in use
// rather than the one we hoped for.
async function reportDirect(): Promise<void> {
  type Pair = {
    type: string;
    state?: string;
    nominated?: boolean;
    currentRoundTripTime?: number;
    remoteCandidateId?: string;
  };
  let addr = "peer";
  let ms = 0;
  try {
    const report = await pc!.getStats();
    report.forEach((raw) => {
      const r = raw as Pair;
      if (r.type !== "candidate-pair" || r.state !== "succeeded") return;
      ms = Math.round((r.currentRoundTripTime ?? 0) * 1000);
      const rem = r.remoteCandidateId
        ? (report.get(r.remoteCandidateId) as { address?: string; port?: number } | undefined)
        : undefined;
      if (rem?.address) addr = `${rem.address}:${rem.port}`;
    });
  } catch (_) {}
  emit({ t: "direct", peer: addr, ms });
}

async function startAudio(): Promise<void> {
  const graph = micGraph();
  if (!graph) return;
  const { ctx, source } = graph;

  if (!workletReady) {
    await ctx.audioWorklet.addModule(workletUrl());
    workletReady = true;
  }

  capture = new AudioWorkletNode(ctx, "capture-worklet");
  capture.port.onmessage = (e) => encode(e.data as Float32Array<ArrayBuffer>);
  source.connect(capture); // never to the destination: the mic is not monitored

  play = new AudioWorkletNode(ctx, "play-worklet");
  play.connect(ctx.destination);

  encoder = new AudioEncoder({
    output: (chunk) => {
      const payload = new Uint8Array(chunk.byteLength);
      chunk.copyTo(payload);
      if (dc?.readyState === "open") dc.send(frame(payload));
    },
    error: (e) => hangup(`encoder: ${e.message}`, true),
  });
  encoder.configure({
    codec: "opus",
    sampleRate: SAMPLE_RATE,
    numberOfChannels: 1,
    bitrate: BITRATE,
    opus: { frameDuration: 5000 },
  });

  decoder = new AudioDecoder({
    output: (audio) => {
      const f32 = new Float32Array(audio.numberOfFrames);
      audio.copyTo(f32, { planeIndex: 0, format: "f32" });
      audio.close();
      let sum = 0;
      for (const s of f32) sum += s * s;
      rxSum += sum / Math.max(1, f32.length);
      rxCount++;
      play?.port.postMessage(f32, [f32.buffer]);
    },
    error: (e) => hangup(`decoder: ${e.message}`, true),
  });
  decoder.configure({ codec: "opus", sampleRate: SAMPLE_RATE, numberOfChannels: 1 });

  levelTimer = setInterval(() => {
    const rms = rxCount > 0 ? Math.sqrt(rxSum / rxCount) : 0;
    rxSum = 0;
    rxCount = 0;
    emit({ t: "peer_level", session: peer, db: Math.max(-60, 20 * Math.log10(rms || 1e-6)) });
  }, LEVEL_EVERY);

  await reportDirect();
}

function workletUrl(): string {
  const src = `
    registerProcessor("capture-worklet", class extends AudioWorkletProcessor {
      constructor() { super(); this.buf = new Float32Array(${FRAME_SAMPLES}); this.n = 0; }
      process(inputs) {
        const ch = inputs[0][0];
        if (!ch) return true;
        for (let i = 0; i < ch.length; i++) {
          this.buf[this.n++] = ch[i];
          if (this.n === ${FRAME_SAMPLES}) {
            this.port.postMessage(this.buf.slice(0));
            this.n = 0;
          }
        }
        return true;
      }
    });
    registerProcessor("play-worklet", class extends AudioWorkletProcessor {
      constructor() { super(); this.ring = new Float32Array(8192); this.r = 0; this.w = 0; this.filled = 0;
        this.port.onmessage = (e) => {
          const d = e.data;
          for (let i = 0; i < d.length; i++) {
            this.ring[this.w] = d[i];
            this.w = (this.w + 1) % this.ring.length;
            this.filled = Math.min(this.filled + 1, this.ring.length);
          }
        };
      }
      process(_, outputs) {
        const out = outputs[0][0];
        // wait for one quantum of buffered audio before starting, then run open loop
        if (this.filled < 256 && this.started !== true) { out.fill(0); return true; }
        this.started = true;
        for (let i = 0; i < out.length; i++) {
          if (this.filled > 0) { out[i] = this.ring[this.r]; this.r = (this.r + 1) % this.ring.length; this.filled--; }
          else out[i] = 0;
        }
        return true;
      }
    });
  `;
  return URL.createObjectURL(new Blob([src], { type: "application/javascript" }));
}

function encode(f32: Float32Array<ArrayBuffer>): void {
  if (!encoder) return;
  tsUs += 5000;
  encoder.encode(
    new AudioData({
      format: "f32",
      sampleRate: SAMPLE_RATE,
      numberOfFrames: f32.length,
      numberOfChannels: 1,
      timestamp: tsUs,
      data: f32,
    }),
  );
}

function frame(payload: Uint8Array): ArrayBuffer {
  seq = (seq + 1) & 0xffff;
  ts = (ts + FRAME_SAMPLES) >>> 0;
  const out = new Uint8Array(12 + payload.length);
  const dv = new DataView(out.buffer);
  dv.setUint8(0, PROTO_VERSION);
  dv.setUint8(1, 0);
  dv.setUint32(2, session, true);
  dv.setUint16(6, seq, true);
  dv.setUint32(8, ts, true);
  out.set(payload, 12);
  return out.buffer;
}

function onChannelData(buf: ArrayBuffer): void {
  if (buf.byteLength < 12 || !decoder) return;
  if (new DataView(buf).getUint8(0) !== PROTO_VERSION) return;
  rxTsUs += 5000;
  decoder.decode(new EncodedAudioChunk({ type: "delta", timestamp: rxTsUs, data: new Uint8Array(buf, 12) }));
}

function teardown(): void {
  if (levelTimer !== undefined) clearInterval(levelTimer);
  levelTimer = undefined;
  try {
    encoder?.close();
  } catch (_) {}
  try {
    decoder?.close();
  } catch (_) {}
  capture?.disconnect();
  play?.disconnect();
  // Drop the handlers before closing: otherwise tearing down on purpose fires
  // dc.onclose and the window reports a hangup nobody suffered.
  if (dc) {
    dc.onclose = null;
    dc.onmessage = null;
    dc.close();
  }
  if (pc) {
    pc.onconnectionstatechange = null;
    pc.close();
  }
  encoder = decoder = null;
  capture = play = null;
  dc = null;
  pc = null;
  peer = 0;
  hostCands = 0;
  seq = ts = tsUs = rxTsUs = 0;
  rxSum = rxCount = 0;
}

// The terminal handler for a dying call, whichever of the channel and the
// connection notices first. An ICE failure is the one hangup worth naming:
// with leak protection gathering no host candidates, a bare "connection
// failed" hides that the browser itself blocked the call.
function hangup(why: string, fatalIn: boolean): void {
  const conn = pc;
  if (!conn) return;
  const iceFailed = conn.connectionState === "failed";
  const fatal = fatalIn || iceFailed;
  let msg = iceFailed ? "connection failed" : why;
  if (iceFailed && hostCands === 0) {
    msg +=
      " - your browser hid every local address (WebRTC leak protection), so peers on your network cannot reach you. In Firefox: about:config, reset media.peerconnection.ice.no_host";
  }
  teardown();
  closeMic();
  emit(fatal ? { t: "ended", why: msg } : { t: "status", text: `idle - ${msg}` });
}

function leave(): void {
  teardown();
  closeMic();
  explained = true;
  ws?.close();
  ws = null;
}

export const webInvoke = async (
  cmd: string,
  args?: Record<string, unknown>,
): Promise<unknown> => {
  switch (cmd) {
    case "audio_devices":
      return audioDevices();
    case "join":
      return join(String(args?.["room"] ?? ""), String(args?.["input"] ?? ""), String(args?.["output"] ?? ""));
    case "leave":
      return leave();
    case "display_name":
      return NAME;
    default:
      throw new Error(`the browser build has no ${cmd}`);
  }
};
