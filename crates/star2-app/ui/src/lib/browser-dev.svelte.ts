// Dev-only stand-in for the Tauri bridge so the UI runs in a plain browser
// (`bun run dev` without the Tauri shell). Never imported in a Tauri build.
import type { AudioDevices, EngineEvent } from "./engine.svelte";

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

export const inBrowser = typeof window !== "undefined" && !("__TAURI_INTERNALS__" in window);

type Listener = (e: EngineEvent) => void;
const listeners = new Set<Listener>();
const timers: ReturnType<typeof setTimeout>[] = [];

const emit = (e: EngineEvent) => listeners.forEach((l) => l(e));
const later = (ms: number, e: EngineEvent) => {
  timers.push(setTimeout(() => emit(e), ms));
};
const peerName = (): string => peerNames[nextPeer++ % peerNames.length] ?? "peer";

export const mockInvoke = (cmd: string, _args?: Record<string, unknown>): unknown => {
  switch (cmd) {
    case "audio_devices":
      return devices;
    case "join": {
      peerSessions.length = 0;
      nextSession = 1;
      later(800, { t: "room_joined", room: "MOCK-ROOM-4F2A" });
      later(1600, { t: "status", text: "punching" });
      later(2600, { t: "direct", peer: "203.0.113.7:51820", ms: 96 });
      later(2600, {
        t: "peer_joined",
        session: (peerSessions.push(nextSession), nextSession++),
        name: peerName(),
      });
      return "MOCK-ROOM-4F2A";
    }
    case "leave":
      timers.forEach(clearTimeout);
      timers.length = 0;
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
