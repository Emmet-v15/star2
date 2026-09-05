// Dev-only stand-in for the Tauri bridge so the UI runs in a plain browser
// (`bun run dev` without the Tauri shell). Never imported in a Tauri build.
import type { AudioDevices, EngineEvent } from "./engine.svelte";
import { inBrowser } from "./mic";

export { inBrowser };

const devices: AudioDevices = {
  input: [
    { name: "Mock Microphone", default: true },
    { name: "Mock Webcam Array", default: false },
  ],
  output: [
    { name: "Mock Speakers", default: true },
    { name: "Mock Headphones", default: false },
  ],
};

const peerNames = ["night-raven", "quiet-fox", "old-harbor", "brass-lantern"];
let nextSession = 1;
let nextPeer = 0;
const peerSessions: number[] = [];
const peerDb = new Map<number, number>();

type Listener = (e: EngineEvent) => void;
const listeners = new Set<Listener>();
const timers: ReturnType<typeof setTimeout>[] = [];

const emit = (e: EngineEvent) => listeners.forEach((l) => l(e));
const later = (ms: number, e: EngineEvent) => {
  timers.push(setTimeout(() => emit(e), ms));
};
const peerName = (): string => peerNames[nextPeer++ % peerNames.length] ?? "peer";

let micTimer: ReturnType<typeof setInterval> | undefined;
let micDb = -50;

const statsFrame = (): EngineEvent => ({
  t: "stats",
  jitter_ms: 1.2,
  target_ms: 40,
  loss_pct: 0,
  out_ms: 12,
  rx_pps: 200,
  play_fps: 200,
  tx_pps: 200,
  mic_db: micDb,
  rtt_ms: 96,
  jb_ms: 35,
  dev_in_ms: 10,
  in_ring_ms: 5,
  enc_ms: 0.8,
  tx_path_ms: 1,
  dev_out_ms: 12,
  rx_path_ms: 1,
  path: "direct",
});

const startMicSimulation = () => {
  micTimer = setInterval(() => {
    micDb += (Math.random() - 0.45) * 12;
    micDb = Math.min(-12, Math.max(-46, micDb + (micDb < -30 ? 3 : 0)));
    emit(statsFrame());
    for (const s of peerSessions) {
      const db = peerDb.get(s) ?? -35;
      let next = db + (Math.random() - 0.42) * 14;
      next = Math.min(-13, Math.max(-45, next + (next < -28 ? 3.5 : 0)));
      peerDb.set(s, next);
      emit({ t: "peer_level", session: s, db: next });
    }
  }, 120);
};

const stopMicSimulation = () => {
  if (micTimer !== undefined) clearInterval(micTimer);
  micTimer = undefined;
  peerDb.clear();
};

export const mockInvoke = (cmd: string, _args?: Record<string, unknown>): unknown => {
  switch (cmd) {
    case "audio_devices":
      return devices;
    case "join": {
      peerSessions.length = 0;
      peerDb.clear();
      nextSession = 1;
      micDb = -32;
      later(800, { t: "room_joined", room: "MOCK-ROOM-4F2A" });
      later(1600, { t: "status", text: "punching" });
      later(2600, { t: "direct", peer: "203.0.113.7:51820", ms: 96 });
      later(2650, statsFrame());
      later(2660, {
        t: "peer_joined",
        session: (peerSessions.push(nextSession), nextSession++),
        name: peerName(),
      });
      timers.push(setTimeout(startMicSimulation, 2700));
      return "MOCK-ROOM-4F2A";
    }
    case "leave":
      timers.forEach(clearTimeout);
      timers.length = 0;
      stopMicSimulation();
      return "mock-machine";
    case "display_name":
      return "mock-machine";
    default:
      throw new Error(`no mock for ${cmd}`);
  }
};

export function listenMock(l: Listener): () => void {
  listeners.add(l);
  return () => listeners.delete(l);
}

export function mockAddPeer(): void {
  const session = nextSession++;
  peerSessions.push(session);
  emit({ t: "peer_joined", session, name: peerName() });
}

export function mockRemovePeer(): void {
  const session = peerSessions.pop();
  if (session === undefined) return;
  nextPeer = Math.max(0, nextPeer - 1);
  emit({ t: "peer_left", session });
}
