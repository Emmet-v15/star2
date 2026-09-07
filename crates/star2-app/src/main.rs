// A GUI app: diagnostics live in star2.log beside the binary, so there is no
// reason to drag an empty console window onto every user's taskbar.
#![windows_subsystem = "windows"]

mod updater;

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use star_voice::{start_call, CallConfig, CallHandle, Event};
use tauri::{AppHandle, Emitter, Manager, State};

const MANIFEST_URL: &str = "https://v15.studio/star2.json";
const UPDATES_WS: &str = "wss://star.v15.studio/star2/updates";
const RENDEZVOUS_URL: &str = "wss://star.v15.studio/star2";
const RENDEZVOUS_TOKEN: &str = "ad7afaabdfe6a6636c3e3e478321039c";
const SEED_MAX_AGE_SECS: u64 = 120;
const LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;

static LOG: OnceLock<Mutex<std::fs::File>> = OnceLock::new();
static STARTED: OnceLock<Instant> = OnceLock::new();

fn log_path(me: &Path) -> PathBuf {
    me.with_extension("log")
}

fn open_log(me: &Path) {
    let _ = STARTED.set(Instant::now());
    let path = log_path(me);
    if std::fs::metadata(&path).map(|m| m.len() > LOG_MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = LOG.set(Mutex::new(f));
    }
    log(&format!("--- star2 {} starting ---", env!("CARGO_PKG_VERSION")));
}

fn log(line: &str) {
    let Some(lock) = LOG.get() else { return };
    let t = STARTED.get().map(Instant::elapsed).unwrap_or_default();
    let Ok(mut f) = lock.lock() else { return };
    let (m, s, ms) = (t.as_secs() / 60, t.as_secs() % 60, t.subsec_millis());
    let _ = writeln!(f, "{m:02}:{s:02}.{ms:03}  {line}");
    let _ = f.flush();
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct SeedFile {
    room: String,
    ts: u64,
    relay: String,
    peer: String,
    devices: Devices,
}

fn seed_path(me: &Path) -> PathBuf {
    me.with_extension("restart-seed.json")
}

fn write_restart_seed(
    me: &Path,
    room: &str,
    devices: Devices,
    route: Option<(std::net::SocketAddr, String)>,
) {
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else { return };
    let seed = SeedFile {
        room: room.to_string(),
        ts: now.as_secs(),
        relay: route.as_ref().map(|(_, r)| r.clone()).unwrap_or_default(),
        peer: route.map(|(p, _)| p.to_string()).unwrap_or_default(),
        devices,
    };
    let Ok(json) = serde_json::to_string(&seed) else { return };
    let tmp = me.with_extension("restart-seed.tmp");
    if std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, seed_path(me))).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn load_restart_seed(me: &Path) -> Option<SeedFile> {
    let path = seed_path(me);
    let txt = std::fs::read_to_string(&path).ok()?;
    let seed: SeedFile = match serde_json::from_str(&txt) {
        Ok(s) => s,
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            return None;
        }
    };
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(now) if now.as_secs().saturating_sub(seed.ts) <= SEED_MAX_AGE_SECS => Some(seed),
        _ => {
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}

fn pc_name() -> Option<String> {
    let raw = gethostname::gethostname().to_string_lossy().into_owned();
    let name = raw.trim().trim_end_matches('.');
    let name = name.strip_suffix(".local").unwrap_or(name);
    let name = name.split('.').next().unwrap_or(name).trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn emit_status(app: &AppHandle, text: String) {
    log(&text);
    let _ = app.emit("engine", Event::Status { text });
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Default, Clone)]
struct Devices {
    input: String,
    output: String,
}

#[derive(Default)]
struct Resume {
    room: Option<String>,
    devices: Devices,
    route: Option<(std::net::SocketAddr, String)>,
}

enum CallMsg {
    Join {
        room: String,
        devices: Devices,
        seed_relay: String,
        seed_peer: String,
        reply: SyncSender<Result<String, String>>,
    },
    Leave { reply: Sender<()> },
    Shutdown { reply: Sender<Resume> },
}

struct App {
    me: PathBuf,
    call_tx: Sender<CallMsg>,
}

fn spawn_call_owner(app: AppHandle) -> Sender<CallMsg> {
    let (tx, rx) = channel::<CallMsg>();
    std::thread::Builder::new()
        .name("call".into())
        .spawn(move || own_calls(app, rx))
        .expect("spawn the call owner");
    tx
}

fn own_calls(app: AppHandle, rx: Receiver<CallMsg>) {
    let mut call: Option<CallHandle> = None;
    let mut room: Option<String> = None;
    let mut chosen = Devices::default();
    for msg in rx {
        match msg {
            CallMsg::Join { room: requested, devices, seed_relay, seed_peer, reply } => {
                if call.is_some() {
                    let _ = reply.send(Err("already in a call".into()));
                    continue;
                }
                let (token, minted) = match star_proto::is_room_token(&requested) {
                    true => (requested.clone(), false),
                    false => (star_proto::new_room_token(&requested), true),
                };

                chosen = devices.clone();
                let cfg = CallConfig {
                    url: env_or("STAR2_URL", RENDEZVOUS_URL),
                    room_token: token.clone(),
                    name: pc_name().unwrap_or_else(|| "anon".into()),
                    token: RENDEZVOUS_TOKEN.into(),
                    input: env_or("STAR2_INPUT", &devices.input),
                    output: env_or("STAR2_OUTPUT", &devices.output),
                    seed_relay,
                    seed_peer,
                    ..Default::default()
                };

                let emitter = app.clone();
                match start_call(cfg, move |e| {
                    match &e {
                        Event::Log { line } => return log(line),
                        Event::Status { text } => log(text),
                        Event::Direct { peer, ms } => log(&format!("direct {peer} in {ms} ms")),
                        Event::RoomJoined { room } => log(&format!("joined {room}")),
                        Event::Ended { why } => log(&format!("ended: {why}")),
                        Event::Stats { .. } => {}
                        Event::PeerJoined { session, name } => {
                            log(&format!("peer {name} (s{session}) in roster"))
                        }
                        Event::PeerLeft { session } => log(&format!("peer s{session} left roster")),
                        Event::PeerLevel { .. } | Event::Spectrum { .. } => {}
                    }
                    let _ = emitter.emit("engine", e);
                }) {
                    Ok(handle) => {
                        call = Some(handle);
                        room = Some(token.clone());
                        if minted {
                            let _ = app.emit(
                                "engine",
                                Event::Status { text: format!("{token}  (share it)") },
                            );
                        }
                        let _ = reply.send(Ok(token));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(format!("{e:#}")));
                    }
                }
            }
            CallMsg::Leave { reply } => {
                if let Some(h) = call.take() {
                    h.stop();
                }
                room = None;
                let _ = reply.send(());
            }
            CallMsg::Shutdown { reply } => {
                let route = call.as_ref().and_then(CallHandle::last_route);
                if let Some(h) = call.take() {
                    h.stop();
                }
                let devices = std::mem::take(&mut chosen);
                let _ = reply.send(Resume { room: room.take(), devices, route });
            }
        }
    }
}

fn request_join(
    state: &App,
    room: &str,
    devices: Devices,
    seed_relay: String,
    seed_peer: String,
) -> Result<String, String> {
    let (reply_tx, reply_rx) = sync_channel(1);
    state
        .call_tx
        .send(CallMsg::Join {
            room: room.to_string(),
            devices,
            seed_relay,
            seed_peer,
            reply: reply_tx,
        })
        .map_err(|_| "call thread gone".to_string())?;
    reply_rx.recv().map_err(|_| "call thread gone".to_string())?
}

#[tauri::command]
fn join(state: State<App>, room: String, input: String, output: String) -> Result<String, String> {
    request_join(&state, room.trim(), Devices { input, output }, String::new(), String::new())
}

#[derive(Serialize)]
struct AudioDevices {
    input: Vec<star_voice::AudioDevice>,
    output: Vec<star_voice::AudioDevice>,
}

#[tauri::command]
fn audio_devices() -> Result<AudioDevices, String> {
    Ok(AudioDevices {
        input: star_voice::input_devices().map_err(|e| format!("{e:#}"))?,
        output: star_voice::output_devices().map_err(|e| format!("{e:#}"))?,
    })
}

#[tauri::command]
fn leave(state: State<App>) -> Result<(), String> {
    let (reply_tx, reply_rx) = channel();
    state.call_tx.send(CallMsg::Leave { reply: reply_tx }).map_err(|_| "call thread gone")?;
    reply_rx.recv().map_err(|_| "call thread gone")?;
    Ok(())
}

#[tauri::command]
fn display_name() -> String {
    pc_name().unwrap_or_else(|| "anon".into())
}

fn restart_into_update(app: &AppHandle) -> ! {
    let state = app.state::<App>();
    let (reply_tx, reply_rx) = channel();
    if let Err(why) = state.call_tx.send(CallMsg::Shutdown { reply: reply_tx }) {
        eprintln!("shutdown send failed: {why}");
    }
    let resume = reply_rx.recv().unwrap_or_default();
    if let Some(room) = resume.room.as_deref() {
        write_restart_seed(&state.me, room, resume.devices, resume.route);
    }
    app.restart()
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn spawn_upkeep(app: AppHandle) {
    std::thread::spawn(move || {
        let state = app.state::<App>();
        let me = state.me.clone();
        let manifest_url = env_or("STAR2_MANIFEST_URL", MANIFEST_URL);
        let updates_ws = env_or("STAR2_UPDATES_WS", UPDATES_WS);

        // Staleness is decided by binary sha256, so a dev build never matches
        // the release manifest: the updater would swap the production binary
        // over the one cargo just built and restart into it. Dev builds never
        // self-update.
        if cfg!(debug_assertions) {
            log("updater disabled: dev build");
        } else {
            match updater::check_at_startup(&me, &manifest_url) {
                updater::Outcome::Installed { version } => {
                    emit_status(&app, format!("update {version} installed - restarting"));
                    restart_into_update(&app);
                }
                updater::Outcome::Failed { why } => {
                    emit_status(&app, format!("update unavailable: {why}"))
                }
                updater::Outcome::Current => {}
            }
        }

        if let Some(seed) = load_restart_seed(&me) {
            let _ = std::fs::remove_file(seed_path(&me));
            let _ = request_join(&state, &seed.room, seed.devices, seed.relay, seed.peer);
        }

        if cfg!(debug_assertions) {
            return;
        }

        loop {
            let swapped = updater::block_on(updater::watch_session(
                &me,
                &updates_ws,
                &manifest_url,
                env!("CARGO_PKG_VERSION"),
            ));
            if matches!(swapped, Ok(true)) {
                restart_into_update(&app);
            }
            std::thread::sleep(updater::reconnect_delay());
        }
    });
}

fn main() {
    let me = std::env::current_exe().expect("locate the running binary");
    open_log(&me);
    // The console is gone, so a panic would otherwise vanish unheard.
    std::panic::set_hook(Box::new(|info| log(&format!("panic: {info}"))));
    star_voice::set_verbose(true);
    tauri::Builder::default()
        .setup(|app| {
            let call_tx = spawn_call_owner(app.handle().clone());
            app.manage(App { me, call_tx });
            spawn_upkeep(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![join, leave, display_name, audio_devices])
        .run(tauri::generate_context!())
        .expect("error while running star2");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_me(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("star2-app-{tag}-{}.exe", std::process::id()))
    }

    #[test]
    fn a_fresh_seed_carries_the_room_and_the_route() {
        let me = temp_me("fresh");
        let _ = std::fs::remove_file(seed_path(&me));

        write_restart_seed(
            &me,
            "general",
            Devices { input: "Yeti".into(), output: "Speakers".into() },
            Some(("203.0.113.7:51820".parse().unwrap(), "empire:40001".into())),
        );
        let s = load_restart_seed(&me).expect("a just-written seed must validate");
        assert_eq!(s.room, "general", "the successor must know which room to rejoin");
        assert_eq!(
            s.peer, "203.0.113.7:51820",
            "the peer endpoint is the whole point of the seed"
        );
        assert_eq!(s.relay, "empire:40001");
        assert_eq!(
            s.devices,
            Devices { input: "Yeti".into(), output: "Speakers".into() },
            "the successor must reopen the devices the user chose, not the system defaults"
        );
        assert!(seed_path(&me).exists(), "loading must not consume a valid seed");

        let _ = std::fs::remove_file(seed_path(&me));
    }

    #[test]
    fn an_expired_or_broken_seed_is_discarded_silently() {
        let me = temp_me("stale");
        let _ = std::fs::remove_file(seed_path(&me));

        assert_eq!(load_restart_seed(&me), None, "no seed file means no seed");

        let ancient = SeedFile {
            room: "x".into(),
            ts: 0,
            relay: String::new(),
            peer: String::new(),
            devices: Devices::default(),
        };
        std::fs::write(seed_path(&me), serde_json::to_string(&ancient).unwrap()).unwrap();
        assert_eq!(load_restart_seed(&me), None, "ts=0 is ancient history");
        assert!(!seed_path(&me).exists(), "a rejected seed must not linger");

        std::fs::write(seed_path(&me), "{not json").unwrap();
        assert_eq!(load_restart_seed(&me), None, "a broken seed is not a crash");
        assert!(!seed_path(&me).exists());

        let _ = std::fs::remove_file(seed_path(&me));
    }
}
