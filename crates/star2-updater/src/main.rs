use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MANIFEST_URL: &str = "https://v15.studio/star2.json";
const UPDATES_WS: &str = "wss://star.v15.studio/star2/updates";
const RECONNECT_DELAY: Duration = Duration::from_secs(10);

const USAGE: &str = "\
star2 - runs the voice engine and keeps it up to date

USAGE:
    star2 [ENGINE OPTIONS...]

Every option is passed straight through to star2-engine, so:

    star2 --room myroom --name me --stats

Runner-specific:
    --updater-help     this (engine options: star2-engine --help)
";

#[derive(Deserialize)]
struct Manifest {
    version: String,
    sha256: String,
    url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--updater-help") {
        print!("{USAGE}");
        return Ok(());
    }
    let _ = rustls::crypto::ring::default_provider().install_default();

    let exe = engine_path()?;
    println!("star2: supervising {}", exe.display());

    if let Some(m) = stale_manifest(&exe).await {
        install(&exe, &m).await;
    }

    let mut sup = Supervisor::new(&exe, &args);

    loop {

        if let Err(e) = run_session(&exe, &args, &mut sup).await {
            eprintln!("  updater: {e}");
        }

        supervise(&exe, &args, &mut sup, RECONNECT_DELAY);
    }
}

async fn run_session(exe: &Path, args: &[String], sup: &mut Supervisor) -> Result<()> {
    let (ws, _) = tokio_tungstenite::connect_async(UPDATES_WS)
        .await
        .context("connect to update channel")?;
    println!("  update channel connected");
    let (_tx, mut rx) = ws.split();

    check_and_apply(exe, args, sup).await;

    let mut live = tokio::time::interval(Duration::from_secs(2));
    live.tick().await;

    loop {
        tokio::select! {
            msg = rx.next() => match msg {
                Some(Ok(_)) => {
                    println!("  update announced");
                    check_and_apply(exe, args, sup).await;
                }
                _ => return Ok(()),
            },

            _ = live.tick() => sup.ensure_running(exe, args),
        }
    }
}

async fn stale_manifest(exe: &Path) -> Option<Manifest> {
    let manifest = match fetch_manifest().await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("  manifest unavailable ({e}) - keeping current engine");
            return None;
        }
    };

    if let Ok(local) = local_sha(exe) {
        if local.eq_ignore_ascii_case(&manifest.sha256) {
            return None;
        }
    }
    Some(manifest)
}

async fn install(exe: &Path, m: &Manifest) {
    println!("  installing build {}", m.version);
    match stage(exe, m).await {
        Ok(tmp) => match swap(exe, &tmp) {
            Ok(()) => println!("  now on {}", m.version),
            Err(e) => eprintln!("  install failed, keeping current build: {e}"),
        },
        Err(e) => eprintln!("  download failed, keeping current build: {e}"),
    }
}

async fn check_and_apply(exe: &Path, args: &[String], sup: &mut Supervisor) {
    let Some(manifest) = stale_manifest(exe).await else { return };
    println!("  downloading build {}", manifest.version);

    let staged = match stage(exe, &manifest).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("  download failed, staying on current build: {e}");
            return;
        }
    };

    println!("  stopping engine to swap in {}", manifest.version);
    sup.stop();

    if let Err(e) = swap(exe, &staged) {
        eprintln!("  install failed: {e}");
    } else {
        println!("  now on {}", manifest.version);
    }

    sup.start_now(exe, args);
}

async fn stage(exe: &Path, m: &Manifest) -> Result<PathBuf> {
    let bytes = reqwest::get(&m.url).await?.error_for_status()?.bytes().await?;

    let got = hex(Sha256::digest(&bytes).as_slice());
    if !got.eq_ignore_ascii_case(&m.sha256) {

        bail!("sha256 mismatch (manifest {}, got {got})", m.sha256);
    }

    let tmp = exe.with_extension("new");
    std::fs::write(&tmp, &bytes).context("write new binary")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .context("mark new binary executable")?;
    }
    Ok(tmp)
}

fn swap(exe: &Path, staged: &Path) -> Result<()> {
    std::fs::rename(staged, exe).context("overwrite engine with staged build")
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
    println!("  starting star2-engine");

    Command::new(exe).args(args).spawn().context("spawn star2-engine")
}

fn spawn_or_warn(exe: &Path, args: &[String]) -> Option<Child> {
    match spawn(exe, args) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("  could not start engine ({e:#}) - will retry after update check");
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
                        "  star2-engine exited ({status}) - restarting in {:.1}s",
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
