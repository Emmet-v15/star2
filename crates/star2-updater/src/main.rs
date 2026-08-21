use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[cfg(target_os = "macos")]
const MANIFEST_URL: &str = "https://v15.studio/star2-macos.json";
#[cfg(target_os = "linux")]
const MANIFEST_URL: &str = "https://v15.studio/star2-linux.json";
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const MANIFEST_URL: &str = "https://v15.studio/star2.json";
const UPDATES_WS: &str = "wss://star.v15.studio/star2/updates";
const RECONNECT_DELAY: Duration = Duration::from_secs(10);

const USAGE: &str = "\
star2 - runs the voice engine and keeps it up to date

USAGE:
    star2                          asks for a room token; blank makes a new room
    star2 --room <ROOM TOKEN>      join a room someone shared the token for
    star2 --create <ROOM NAME>     make a room token, print it, join it

Every other option is passed straight through to star2-engine, so:

    star2 --room gaming-k3n7qp2xza --stats

Runner-specific:
    --updater-help     this (engine options: star2-engine --help)
";

#[derive(Deserialize)]
struct Manifest {
    version: String,
    sha256: String,
    url: String,
    #[serde(default)]
    runner_sha256: String,
    #[serde(default)]
    runner_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--updater-help") {
        print!("{USAGE}");
        return Ok(());
    }
    let mut args = resolve_room(args);
    if !args.iter().any(|a| a == "--room") {
        if let Some(token) = ask_for_room() {
            args.push("--room".into());
            args.push(token);
        }
    }
    let _ = rustls::crypto::ring::default_provider().install_default();

    let exe = engine_path()?;
    let me = std::env::current_exe().context("locate the running runner")?;
    println!("[star2] {} supervising {}", env!("CARGO_PKG_VERSION"), exe.display());
    let _ = std::fs::remove_file(me.with_extension("old"));

    if let Some(m) = fetch_or_warn().await {
        if replace_runner(&me, &m, &args, None).await {
            return Ok(());
        }
        if is_stale(&exe, &m.sha256) {
            install(&exe, &m).await;
        }
    }

    let mut sup = Supervisor::new(&exe, &args);

    loop {

        match run_session(&exe, &me, &args, &mut sup).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => eprintln!("[star2] updater: {e}"),
        }

        supervise(&exe, &args, &mut sup, RECONNECT_DELAY);
    }
}

async fn run_session(
    exe: &Path,
    me: &Path,
    args: &[String],
    sup: &mut Supervisor,
) -> Result<bool> {
    let (ws, _) = tokio_tungstenite::connect_async(UPDATES_WS)
        .await
        .context("connect to update channel")?;
    println!("[star2] update channel connected");
    let (mut tx_ws, mut rx) = ws.split();
    let _ = tx_ws.send(Message::Text(env!("CARGO_PKG_VERSION").into())).await;

    if check_and_apply(exe, me, args, sup).await {
        return Ok(true);
    }

    let mut live = tokio::time::interval(Duration::from_secs(2));
    live.tick().await;

    loop {
        tokio::select! {
            msg = rx.next() => match msg {
                Some(Ok(_)) => {
                    println!("[star2] update announced");
                    if check_and_apply(exe, me, args, sup).await {
                        return Ok(true);
                    }
                }
                _ => return Ok(false),
            },

            _ = live.tick() => sup.ensure_running(exe, args),
        }
    }
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

async fn fetch_or_warn() -> Option<Manifest> {
    match fetch_manifest().await {
        Ok(m) => Some(m),
        Err(e) => {
            eprintln!("[star2] manifest unavailable ({e}) - keeping current build");
            None
        }
    }
}

fn is_stale(path: &Path, want: &str) -> bool {
    match local_sha(path) {
        Ok(local) => !local.eq_ignore_ascii_case(want),
        Err(_) => true,
    }
}

async fn replace_runner(
    me: &Path,
    m: &Manifest,
    args: &[String],
    sup: Option<&mut Supervisor>,
) -> bool {
    if m.runner_sha256.is_empty() || m.runner_url.is_empty() {
        return false;
    }

    if local_sha(me).map(|l| l.eq_ignore_ascii_case(&m.runner_sha256)).unwrap_or(true) {
        return false;
    }

    println!("[star2] downloading runner {}", m.version);
    let staged = match stage(&me.with_extension("new"), &m.runner_url, &m.runner_sha256).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[star2] runner download failed, staying on current runner: {e}");
            return false;
        }
    };

    println!("[star2] stopping engine to swap in runner {}", m.version);
    if let Some(sup) = sup {
        sup.stop();
    }

    if let Err(e) = swap_self(me, &staged) {
        eprintln!("[star2] runner install failed, staying on current runner: {e}");
        let _ = std::fs::remove_file(&staged);
        return false;
    }

    println!("[star2] runner now on {} - relaunching", m.version);
    match Command::new(me).args(args).spawn() {
        Ok(_) => true,
        Err(e) => {
            eprintln!("[star2] relaunch failed ({e}) - start star2 again by hand");
            true
        }
    }
}

async fn install(exe: &Path, m: &Manifest) {
    println!("[star2] installing build {}", m.version);
    match stage(&exe.with_extension("new"), &m.url, &m.sha256).await {
        Ok(tmp) => match swap(exe, &tmp) {
            Ok(()) => println!("[star2] engine now on {}", m.version),
            Err(e) => eprintln!("[star2] install failed, keeping current build: {e}"),
        },
        Err(e) => eprintln!("[star2] download failed, keeping current build: {e}"),
    }
}

async fn check_and_apply(
    exe: &Path,
    me: &Path,
    args: &[String],
    sup: &mut Supervisor,
) -> bool {
    let Some(manifest) = fetch_or_warn().await else { return false };

    if replace_runner(me, &manifest, args, Some(sup)).await {
        return true;
    }

    if !is_stale(exe, &manifest.sha256) {
        return false;
    }
    println!("[star2] downloading build {}", manifest.version);

    let staged = match stage(&exe.with_extension("new"), &manifest.url, &manifest.sha256).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[star2] download failed, staying on current build: {e}");
            return false;
        }
    };

    println!("[star2] stopping engine to swap in {}", manifest.version);
    sup.stop();

    if let Err(e) = swap(exe, &staged) {
        eprintln!("[star2] install failed: {e}");
    } else {
        println!("[star2] now on {}", manifest.version);
    }

    sup.start_now(exe, args);
    false
}

async fn stage(tmp: &Path, url: &str, sha: &str) -> Result<PathBuf> {
    let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;

    let got = hex(Sha256::digest(&bytes).as_slice());
    if !got.eq_ignore_ascii_case(sha) {

        bail!("sha256 mismatch (manifest {sha}, got {got})");
    }

    std::fs::write(tmp, &bytes).context("write new binary")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o755))
            .context("mark new binary executable")?;
    }
    Ok(tmp.to_path_buf())
}

fn swap(exe: &Path, staged: &Path) -> Result<()> {
    std::fs::rename(staged, exe).context("overwrite engine with staged build")
}

fn swap_self(me: &Path, staged: &Path) -> Result<()> {
    let old = me.with_extension("old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(me, &old).context("move the running runner aside")?;
    if let Err(e) = std::fs::rename(staged, me) {
        let _ = std::fs::rename(&old, me);
        return Err(e).context("move the new runner into place");
    }
    Ok(())
}

async fn fetch_manifest() -> Result<Manifest> {
    let txt = reqwest::get(MANIFEST_URL).await?.error_for_status()?.text().await?;
    Ok(serde_json::from_str(&txt).context("parse manifest")?)
}

fn local_sha(exe: &Path) -> Result<String> {
    Ok(hex(Sha256::digest(std::fs::read(exe)?).as_slice()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn engine_path() -> Result<PathBuf> {
    let dir = std::env::current_exe()?
        .parent()
        .context("runner has no parent directory")?
        .to_path_buf();
    Ok(dir.join(if cfg!(windows) { "star2-engine.exe" } else { "star2-engine" }))
}

fn spawn(exe: &Path, args: &[String]) -> Result<Child> {
    println!("[star2] starting star2-engine");

    Command::new(exe).args(args).spawn().context("spawn star2-engine")
}

fn spawn_or_warn(exe: &Path, args: &[String]) -> Option<Child> {
    match spawn(exe, args) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("[star2] could not start engine ({e:#}) - will retry after update check");
            None
        }
    }
}

struct Supervisor {
    child: Option<Child>,

    next_spawn: Instant,

    backoff: Duration,

    started: Instant,
}

const RESTART_MIN: Duration = Duration::from_millis(500);
const RESTART_MAX: Duration = Duration::from_secs(30);

const HEALTHY_RUN: Duration = Duration::from_secs(20);

impl Supervisor {
    fn new(exe: &Path, args: &[String]) -> Self {
        Self {
            child: spawn_or_warn(exe, args),
            next_spawn: Instant::now(),
            backoff: RESTART_MIN,
            started: Instant::now(),
        }
    }

    fn ensure_running(&mut self, exe: &Path, args: &[String]) {
        if let Some(c) = self.child.as_mut() {
            match c.try_wait() {
                Ok(Some(status)) => {
                    if self.started.elapsed() >= HEALTHY_RUN {
                        self.backoff = RESTART_MIN;
                    } else {
                        self.backoff = (self.backoff * 2).min(RESTART_MAX);
                    }
                    self.next_spawn = Instant::now() + self.backoff;
                    println!(
                        "[star2] engine exited ({status}) - restarting in {:.1}s",
                        self.backoff.as_secs_f32()
                    );
                    self.child = None;
                }
                _ => return,
            }
        }
        if self.child.is_none() && Instant::now() >= self.next_spawn {
            self.child = spawn_or_warn(exe, args);
            self.started = Instant::now();
        }
    }

    fn stop(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.child = None;
    }

    fn start_now(&mut self, exe: &Path, args: &[String]) {
        self.backoff = RESTART_MIN;
        self.next_spawn = Instant::now();
        self.child = spawn_or_warn(exe, args);
        self.started = Instant::now();
    }
}

fn supervise(exe: &Path, args: &[String], sup: &mut Supervisor, dur: Duration) {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        sup.ensure_running(exe, args);
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_room;

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
}
