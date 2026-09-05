import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { inBrowser, listenMock, mockInvoke } from "./browser-dev.svelte";

export const invoke = async (
  cmd: string,
  args?: Record<string, unknown>,
): Promise<unknown> => (inBrowser ? mockInvoke(cmd, args) : tauriInvoke(cmd, args));

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
      jb_ms: number;
      dev_in_ms: number;
      in_ring_ms: number;
      enc_ms: number;
      tx_path_ms: number;
      dev_out_ms: number;
      rx_path_ms: number;
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
    const devs = (await invoke("audio_devices")) as AudioDevices;
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
    const token = (await invoke("join", {
      room: roomInput,
      input: call.input,
      output: call.output,
    })) as string;
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

export async function listenEngine(onEvent: (e: EngineEvent) => void): Promise<UnlistenFn> {
  if (inBrowser) return listenMock(onEvent);
  return listen<EngineEvent>("engine", (e) => onEvent(e.payload));
}
