import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type AudioDevice = { name: string; default: boolean };
export type AudioDevices = { input: AudioDevice[]; output: AudioDevice[] };

export type EngineEvent =
  | { t: "room_joined"; room: string }
  | { t: "direct"; peer: string; ms: number }
  | { t: "ended"; why: string }
  | { t: "status"; text: string }
  | {
      t: "stats";
      jitter_ms: number;
      target_ms: number;
      loss_pct: number;
      out_ms: number;
      rx_pps: number;
      play_fps: number;
      tx_pps: number;
      mic_db: number;
      rtt_ms: number;
      path: string;
    }
  | { t: "log"; line: string };

export type Phase = "idle" | "connecting" | "live" | "down";

export const call = $state({
  phase: "idle" as Phase,
  status: "idle",
  room: localStorage.getItem("star2.room") ?? "",
  fatal: "",
  devices: { input: [] as AudioDevice[], output: [] as AudioDevice[] },
  input: localStorage.getItem("star2.in") ?? "",
  output: localStorage.getItem("star2.out") ?? "",
});

export async function refreshDevices(): Promise<void> {
  try {
    const devs: AudioDevices = await invoke("audio_devices");
    call.devices = devs;
    for (const key of ["input", "output"] as const) {
      const remembered = call[key];
      // A remembered name that is no longer plugged in would silently match
      // nothing; star-voice treats that as an error, so forget it instead.
      if (remembered && !devs[key].some((d) => d.name === remembered)) {
        call[key] = "";
        localStorage.removeItem(`star2.${key}`);
      }
    }
  } catch (e) {
    call.fatal = `audio devices: ${e}`;
  }
}

export async function join(roomInput: string): Promise<void> {
  call.fatal = "";
  call.phase = "connecting";
  try {
    const token: string = await invoke("join", {
      room: roomInput,
      input: call.input,
      output: call.output,
    });
    if (!token) throw new Error("engine returned no room token");
    call.room = token;
    localStorage.setItem("star2.room", token);
  } catch (e) {
    call.fatal = String(e);
    call.phase = "down";
  }
}

export async function leave(): Promise<void> {
  try {
    await invoke("leave");
  } catch (_) {}
  call.fatal = "";
  call.phase = "idle";
  call.status = "idle";
  await refreshDevices();
}

export async function copyRoom(): Promise<void> {
  try {
    await navigator.clipboard.writeText(call.room);
  } catch (_) {}
}

export function listenEngine(onEvent: (e: EngineEvent) => void): Promise<UnlistenFn> {
  return listen<EngineEvent>("engine", (e) => onEvent(e.payload));
}
