use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use star2_engine::{start_call, CallConfig, Event};
use tokio_tungstenite::tungstenite::Message;

const MANIFEST_URL: &str = "https://v15.studio/star2.json";
const UPDATES_WS: &str = "wss://star.v15.studio/star2/updates";
const RECONNECT_DELAY: Duration = Duration::from_secs(10);

static RESTART: AtomicBool = AtomicBool::new(false);

const USAGE: &str = "\
star2 - P2P voice call (48 kHz Opus, direct UDP), keeps itself up to date

USAGE:
    star2                          asks for a room token; blank makes a new room
    star2 --room <TOKEN>           join a room someone shared the token for
    star2 --create <NAME>          make a room token, print it, join it

OPTIONS:
    --url <URL>        signal server           [default: wss://star.v15.studio/star2]
    --name <NAME>      your display name                       [default: this PC's name]
    --token <TOKEN>    shared secret                           [default: baked in]
    --stereo           send 2 channels instead of 1
    --bitrate <BPS>    Opus bitrate                            [default: 128000 mono / 256000 stereo]
    --input <SUBSTR>   input device name match                 [default: system default]
    --output <SUBSTR>  output device name match                [default: system default]
    --dev-buf <MS>     device buffer request, 0 = default      [default: 0]
    --stats            print the 1 Hz jitter/loss/rate readout
    --verbose          print the audio/punching/jitter internals
    --list-devices     print audio devices and exit
    -h, --help         this
";

#[derive(Deserialize)]
struct Manifest {
    version: String,
    sha256: String,
    url: String,
}

fn main() -> Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    if raw.iter().any(|a| a == "--list-devices") {
        return list_devices();
    }

    let _ = rustls::crypto::ring::default_provider().install_default();
    let me = std::env::current_exe().context("locate the running binary")?;
    let _ = std::fs::remove_file(me.with_extension("old"));

    if block_on(check_for_update(&me)) {
        return relaunch(&me, &raw);
    }

    let mut args = resolve_room(raw);
    if !args.iter().any(|a| a == "--room") {
        if let Some(token) = ask_for_room() {
            args.push("--room".into());
            args.push(token);
        }
    }

    let (cfg, stats) = parse(&args)?;
    println!(
        "[star2] {} joining {} as {:?} ({}, {} kbps)",
        env!("CARGO_PKG_VERSION"),
        cfg.room_token,
        cfg.name,
        if cfg.stereo { "stereo" } else { "mono" },
        cfg.bitrate / 1000
    );

    let (tx, rx) = channel();
    let call = start_call(cfg, move |e| {
        let _ = tx.send(e);
    })?;

    watch_for_updates(me.clone());

    loop {
        if RESTART.load(Ordering::Relaxed) {
            println!("[star2] restarting into the new build");
            call.stop();
            return relaunch(&me, &args);
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Event::Status(s)) => println!("[star2] {s}"),
            Ok(Event::Direct(_)) => {}
            Ok(Event::Stats { jitter_ms, target_ms, loss_pct, out_ms, rx_pps, play_fps }) => {
                if stats {
                    println!("  jitter {jitter_ms:.1}ms  buf {target_ms:.0}ms  loss {loss_pct:.1}%  out {out_ms:.0}ms  rx {rx_pps}/s  play {play_fps}/s");
                }
            }
            Ok(Event::Ended(why)) => {
                println!("[star2] fatal: {why}");
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn parse(args: &[String]) -> Result<(CallConfig, bool)> {
    let mut cfg = CallConfig::default();
    let mut bitrate: Option<i32> = None;
    let mut stats = false;
    let mut it = args.iter().cloned();
    while let Some(a) = it.next() {
        let mut val = || it.next().unwrap_or_default();
        match a.as_str() {
            "--url" => cfg.url = val(),
            "--room" => cfg.room_token = val(),
            "--name" => cfg.name = val(),
            "--token" => cfg.token = val(),
            "--stereo" => cfg.stereo = true,
            "--bitrate" => bitrate = val().parse().ok(),
            "--input" => cfg.input = val(),
            "--output" => cfg.output = val(),
            "--dev-buf" => cfg.dev_buf_ms = val().parse().unwrap_or(0),
            "--stats" => stats = true,
            "--verbose" => star2_engine::set_verbose(true),
            other => {
                eprintln!("unknown argument {other:?}\n");
                print!("{USAGE}");
                std::process::exit(2);
            }
        }
    }
    cfg.bitrate = bitrate.unwrap_or(if cfg.stereo { 256_000 } else { 128_000 });
    if cfg.name == "anon" {
        if let Some(h) = pc_name() {
            cfg.name = h;
        }
    }
    Ok((cfg, stats))
}

fn relaunch(me: &Path, args: &[String]) -> Result<()> {
    let status = Command::new(me)
        .args(args)
        .status()
        .context("relaunch into the new build - start star2 again by hand")?;
    std::process::exit(status.code().unwrap_or(0));
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a tokio runtime")
        .block_on(f)
}

fn watch_for_updates(me: PathBuf) {
    std::thread::spawn(move || {
        block_on(async {
            loop {
                if let Err(e) = watch_session(&me).await {
                    eprintln!("[star2] update channel: {e}");
                }
                if RESTART.load(Ordering::Relaxed) {
                    return;
                }
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        })
    });
}

async fn watch_session(me: &Path) -> Result<()> {
    let (ws, _) = tokio_tungstenite::connect_async(UPDATES_WS)
        .await
        .context("connect to update channel")?;
    let (mut tx, mut rx) = ws.split();
    let _ = tx.send(Message::Text(env!("CARGO_PKG_VERSION").into())).await;

    if check_for_update(me).await {
        RESTART.store(true, Ordering::Relaxed);
        return Ok(());
    }
    while let Some(msg) = rx.next().await {
        msg?;
        if check_for_update(me).await {
            RESTART.store(true, Ordering::Relaxed);
            return Ok(());
        }
    }
    Ok(())
}

async fn check_for_update(me: &Path) -> bool {
    let manifest = match fetch_manifest().await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[star2] manifest unavailable ({e}) - keeping this build");
            return false;
        }
    };
    if !is_stale(me, &manifest.sha256) {
        return false;
    }

    println!("[star2] downloading {}", manifest.version);
    let staged = match stage(&me.with_extension("new"), &manifest.url, &manifest.sha256).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[star2] download failed, keeping this build: {e}");
            return false;
        }
    };
    if let Err(e) = swap_self(me, &staged) {
        eprintln!("[star2] install failed, keeping this build: {e}");
        let _ = std::fs::remove_file(&staged);
        return false;
    }
    println!("[star2] installed {}", manifest.version);
    true
}

async fn fetch_manifest() -> Result<Manifest> {
    let txt = reqwest::get(MANIFEST_URL).await?.error_for_status()?.text().await?;
    serde_json::from_str(&txt).context("parse manifest")
}

async fn stage(tmp: &Path, url: &str, sha: &str) -> Result<PathBuf> {
    let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;
    let got = hex(Sha256::digest(&bytes).as_slice());
    if !got.eq_ignore_ascii_case(sha) {
        bail!("sha256 mismatch (manifest {sha}, got {got})");
    }
    std::fs::write(tmp, &bytes).context("write the new binary")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o755))
            .context("mark the new binary executable")?;
    }
    Ok(tmp.to_path_buf())
}

fn swap_self(me: &Path, staged: &Path) -> Result<()> {
    let old = me.with_extension("old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(me, &old).context("move the running binary aside")?;
    if let Err(e) = std::fs::rename(staged, me) {
        let _ = std::fs::rename(&old, me);
        return Err(e).context("move the new binary into place");
    }
    Ok(())
}

fn is_stale(path: &Path, want: &str) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => !hex(Sha256::digest(&bytes).as_slice()).eq_ignore_ascii_case(want),
        Err(_) => true,
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn ask_for_room() -> Option<String> {
    print!("room token (blank to create a new room): ");
    let _ = std::io::stdout().flush();

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).ok()? == 0 {
        return None;
    }

    let typed = line.trim();
    if star2_proto::is_room_token(typed) {
        return Some(typed.to_string());
    }

    let token = star2_proto::new_room_token(typed);
    println!();
    println!("  your room token - share it, they paste it at the same prompt:");
    println!();
    println!("      {token}");
    println!();
    Some(token)
}

fn resolve_room(args: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut it = args.into_iter();
    let mut created: Option<String> = None;
    let mut existing: Option<String> = None;

    while let Some(a) = it.next() {
        match a.as_str() {
            "--create" => {
                let token = star2_proto::new_room_token(&it.next().unwrap_or_default());
                println!();
                println!("  room {:?} created - the token is:", star2_proto::room_label(&token));
                println!();
                println!("      {token}");
                println!();
                println!("  share it - they join with:  star2 --room {token}");
                println!("  --create mints a NEW token every run, so use --room to return to it");
                println!();
                created = Some(token);
            }
            "--room" => existing = it.next(),
            _ => out.push(a),
        }
    }

    if let Some(token) = created.or(existing) {
        out.push("--room".into());
        out.push(token);
    }
    out
}

fn pc_name() -> Option<String> {
    let raw = gethostname::gethostname().to_string_lossy().into_owned();
    let name = raw.trim().trim_end_matches('.');
    let name = name.strip_suffix(".local").unwrap_or(name);
    let name = name.split('.').next().unwrap_or(name).trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn list_devices() -> Result<()> {
    let host = cpal::default_host();
    let dflt_in = host.default_input_device().and_then(|d| d.name().ok()).unwrap_or_default();
    let dflt_out = host.default_output_device().and_then(|d| d.name().ok()).unwrap_or_default();
    println!("inputs:");
    for d in host.input_devices()? {
        let n = d.name().unwrap_or_default();
        println!("  {}{}", n, if n == dflt_in { "  (default)" } else { "" });
    }
    println!("outputs:");
    for d in host.output_devices()? {
        let n = d.name().unwrap_or_default();
        println!("  {}{}", n, if n == dflt_out { "  (default)" } else { "" });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse, resolve_room};

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn create_becomes_a_resolved_room() {
        let out = resolve_room(v(&["--create", "Friday Night"]));
        assert_eq!(out[0], "--room");
        assert_eq!(star2_proto::room_label(&out[1]), "friday-night");
        assert!(star2_proto::is_room_token(&out[1]));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn other_args_pass_through_in_order() {
        let out = resolve_room(v(&["--stats", "--create", "x", "--name", "me"]));
        assert_eq!(out[..4], v(&["--stats", "--name", "me", "--room"])[..]);
    }

    #[test]
    fn without_create_the_room_is_preserved() {
        let out = resolve_room(v(&["--room", "gaming-k3n7qp2xza", "--stats"]));
        assert_eq!(out, v(&["--stats", "--room", "gaming-k3n7qp2xza"]));
    }

    #[test]
    fn a_plain_room_name_still_works() {
        let out = resolve_room(v(&["--room", "general"]));
        assert_eq!(out, v(&["--room", "general"]));
    }

    #[test]
    fn a_dropped_room_flag_leaves_no_stray_value() {
        let out = resolve_room(v(&["--room", "old-k3n7qp2xza", "--create", "new"]));
        assert!(!out.iter().any(|a| a == "old-k3n7qp2xza"));
        for pair in out.chunks(2) {
            assert!(pair[0].starts_with("--"), "stray value {:?} in {out:?}", pair[0]);
        }
    }

    #[test]
    fn create_supersedes_an_existing_room() {
        let out = resolve_room(v(&["--room", "old-k3n7qp2xza", "--create", "new"]));
        assert_eq!(out.iter().filter(|a| *a == "--room").count(), 1);
        assert_eq!(star2_proto::room_label(out.last().unwrap()), "new");
    }

    #[test]
    fn there_is_exactly_one_room_vocabulary() {
        let out = resolve_room(v(&["--create", "gaming"]));
        assert!(out.contains(&"--room".to_string()));
        assert!(!out.contains(&"--join".to_string()));
        assert!(!out.contains(&"--create".to_string()));
    }

    #[test]
    fn resolved_args_are_stable_across_restarts() {
        let once = resolve_room(v(&["--create", "gaming"]));
        assert_eq!(resolve_room(once.clone()), once);
        assert_eq!(resolve_room(resolve_room(once.clone())), once);
    }

    #[test]
    fn mono_is_the_default_and_stereo_is_opt_in() {
        let (mono, _) = parse(&v(&["--room", "general"])).unwrap();
        assert!(!mono.stereo);
        assert_eq!(mono.bitrate, 128_000);

        let (stereo, _) = parse(&v(&["--room", "general", "--stereo"])).unwrap();
        assert!(stereo.stereo);
        assert_eq!(stereo.bitrate, 256_000);
    }

    #[test]
    fn an_explicit_bitrate_wins_over_the_channel_default() {
        let (cfg, _) = parse(&v(&["--stereo", "--bitrate", "96000"])).unwrap();
        assert_eq!(cfg.bitrate, 96_000);
    }
}
