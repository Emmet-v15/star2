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

export const inBrowser = typeof window !== "undefined" && !("__TAURI_INTERNALS__" in window);

type Listener = (e: EngineEvent) => void;
const listeners = new Set<Listener>();
const timers: ReturnType<typeof setTimeout>[] = [];

export const mockInvoke = (cmd: string, _args?: Record<string, unknown>): unknown => {
  switch (cmd) {
    case "audio_devices":
      return devices;
    case "join": {
      const at = (ms: number, e: EngineEvent) => {
        timers.push(setTimeout(() => listeners.forEach((l) => l(e)), ms));
      };
      at(800, { t: "room_joined", room: "MOCK-ROOM-4F2A" });
      at(1600, { t: "status", text: "punching" });
      at(2600, { t: "direct", peer: "203.0.113.7:51820", ms: 96 });
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
