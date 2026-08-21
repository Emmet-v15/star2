//! star2 - the runner. This is the binary a user downloads and keeps running; the
//! voice engine itself is `star2-engine`, which this supervises.
//!
//! It does two jobs, both of which exist so a call can "just keep running":
//!   1. **Supervise** `star2-engine`: launch it, and relaunch it if it exits.
//!   2. **Update** it: hold a WebSocket to the signal server, and when a new build
//!      is announced, fetch the manifest, verify the hash, swap the binary and
//!      restart the child.
//!
//! Updates are push-only. The hash is also checked once when the socket connects,
//! which is what catches a build shipped while this was offline; if the socket
//! drops, we reconnect, and that reconnect check covers whatever was missed.
//!
//! The split also means the frequently-changing part (the engine) is the part that
//! auto-updates, while the supervisor - which must survive the swap - rarely moves.
//!
//! Why a separate executable: Windows will not let a running image be overwritten.
//! It *will* let one be renamed, which is the trick used below - but the child has
//! to be stopped and restarted regardless, and something has to outlive it to do
//! that. That something is this.
//!
//! Trust model: the server nudge carries no payload beyond "go look again". The
//! manifest and binary are fetched over TLS from v15.studio and checked against the
//! manifest's SHA-256, so a forged nudge can at worst cause a redundant re-download.
//! It is NOT a signature - anyone who can write the webroot can ship a build. That
//! is the same trust boundary as the binary you originally downloaded from there.

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

    // Verify BEFORE running: there is no point starting a binary we are about to
    // replace, and checking first makes a missing or corrupt engine self-healing -
    // both cases simply fail the hash comparison and get reinstalled. This is also
    // the first-install path, so a user only needs star2 itself to bootstrap.
    if let Some(m) = stale_manifest(&exe).await {
        install(&exe, &m).await;
    }

    // Still not fatal if it won't start: the next check may well fix it.
    let mut sup = Supervisor::new(&exe, &args);

    loop {
        // Connect, then serve nudges until the socket dies. A check runs on connect
        // too, which covers the case where a build landed while we were offline.
        if let Err(e) = run_session(&exe, &args, &mut sup).await {
            eprintln!("  updater: {e}");
        }
        // Supervise across the reconnect gap so a dead child isn't left dead.
        supervise(&exe, &args, &mut sup, RECONNECT_DELAY);
    }
}

/// Hold the update socket, checking on connect and on every nudge, while keeping
/// the child alive. Returns when the socket closes.
async fn run_session(exe: &Path, args: &[String], sup: &mut Supervisor) -> Result<()> {
    let (ws, _) = tokio_tungstenite::connect_async(UPDATES_WS)
        .await
        .context("connect to update channel")?;
    println!("  update channel connected");
    let (_tx, mut rx) = ws.split();

    check_and_apply(exe, args, sup).await;

    // Interval, not sleep: `select!` cancels the losing branches on every iteration,
    // so a `sleep` here would be restarted each time round and never fire.
    let mut live = tokio::time::interval(Duration::from_secs(2));
    live.tick().await; // the first tick completes immediately - discard it

    loop {
        tokio::select! {
            msg = rx.next() => match msg {
                Some(Ok(_)) => {
                    println!("  update announced");
                    check_and_apply(exe, args, sup).await;
                }
                _ => return Ok(()), // closed or errored; caller reconnects
            },
            // Process supervision only - restart the child if it died on its own.
            // This never touches the network.
            _ = live.tick() => sup.ensure_running(exe, args),
        }
    }
}

/// Fetch the manifest and return it ONLY if the on-disk engine doesn't match it.
/// `None` means "nothing to do" - either we're current, or the manifest is
/// unreachable and we should keep running what we have rather than refuse to start.
async fn stale_manifest(exe: &Path) -> Option<Manifest> {
    let manifest = match fetch_manifest().await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("  manifest unavailable ({e}) - keeping current engine");
            return None;
        }
    };
    // A missing or unreadable binary hashes to nothing, which correctly reads as
    // "not current" and triggers a reinstall - that is the self-repair path.
    if let Ok(local) = local_sha(exe) {
        if local.eq_ignore_ascii_case(&manifest.sha256) {
            return None;
        }
    }
    Some(manifest)
}

/// Download, verify and install `m`. Used at startup, when no engine is running
/// yet so there is nothing to stop.
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

/// The push path: hash differs -> stop the engine -> swap it -> start it again on
/// the same arguments, so it rejoins the same room.
///
/// Stop before swapping. Windows would allow renaming the running image out of the
/// way, but killing first means the file is untouched by anyone when we replace it,
/// which is one less thing to be subtle about.
async fn check_and_apply(exe: &Path, args: &[String], sup: &mut Supervisor) {
    let Some(manifest) = stale_manifest(exe).await else { return };
    println!("  downloading build {}", manifest.version);

    // Download and verify FIRST, while the engine keeps running. This is the slow
    // part, and there is no reason to interrupt a call for it - if the download or
    // the hash check fails we simply carry on, having disturbed nothing.
    let staged = match stage(exe, &manifest).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("  download failed, staying on current build: {e}");
            return;
        }
    };

    // Only now stop the engine: from here it is a rename and a relaunch.
    println!("  stopping engine to swap in {}", manifest.version);
    sup.stop();

    if let Err(e) = swap(exe, &staged) {
        eprintln!("  install failed: {e}");
    } else {
        println!("  now on {}", manifest.version);
    }
    // Same `args` the user launched us with - so it rejoins the same room.
    sup.start_now(exe, args);
}

/// Download, verify, and move into place. The download lands beside the target so
/// the final move is same-volume (and therefore atomic).
/// Download and verify into a staging file beside the target.
///
/// Deliberately does NOT touch the running binary: this is the slow part (seconds
/// of network), so it happens while the engine is still up and on a call. Only the
/// rename in [`swap`] needs the engine stopped.
async fn stage(exe: &Path, m: &Manifest) -> Result<PathBuf> {
    let bytes = reqwest::get(&m.url).await?.error_for_status()?.bytes().await?;

    let got = hex(Sha256::digest(&bytes).as_slice());
    if !got.eq_ignore_ascii_case(&m.sha256) {
        // Refusing here is the whole point of the manifest: a truncated or
        // tampered download must never be executed. Staging separately means we
        // find this out BEFORE stopping a working engine.
        bail!("sha256 mismatch (manifest {}, got {got})", m.sha256);
    }

    let tmp = exe.with_extension("new");
    std::fs::write(&tmp, &bytes).context("write new binary")?;

    // Unix drops a freshly written file at 0644, so a downloaded engine would be
    // non-executable and every launch would fail with EACCES. Windows has no
    // execute bit and infers from the extension, so this is Unix-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .context("mark new binary executable")?;
    }
    Ok(tmp)
}

/// Overwrite the engine with the staged file. One rename, so the window where no
/// engine exists is microseconds rather than the length of a download.
///
/// The engine is already stopped by this point, so nothing holds the file and a
/// plain rename replaces it (`MoveFileEx` with replace-existing on Windows, an
/// atomic same-directory rename on unix). No backup copy is needed: a rename
/// either happens or it doesn't, and if it doesn't the old engine is untouched.
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

/// The engine lives beside the runner, not on PATH - both arrive together.
fn engine_path() -> Result<PathBuf> {
    let dir = std::env::current_exe()?
        .parent()
        .context("runner has no parent directory")?
        .to_path_buf();
    Ok(dir.join(if cfg!(windows) { "star2-engine.exe" } else { "star2-engine" }))
}

fn spawn(exe: &Path, args: &[String]) -> Result<Child> {
    println!("  starting star2-engine");
    // Inherit stdio: the engine's output is the UI, and this is just a wrapper.
    Command::new(exe).args(args).spawn().context("spawn star2-engine")
}

/// Spawn, reporting failure rather than propagating it. `None` means "not running",
/// which the update check treats as a reason to reinstall.
fn spawn_or_warn(exe: &Path, args: &[String]) -> Option<Child> {
    match spawn(exe, args) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("  could not start engine ({e:#}) - will retry after update check");
            None
        }
    }
}

/// Restart bookkeeping.
///
/// A crash-looping engine must NOT be respawned flat out. Restarting instantly
/// turned a transient room condition into ten restarts in seventeen seconds -
/// and because each restart opened a new session, the restarts were themselves
/// what kept the condition true. Backoff bounds any such loop regardless of what
/// causes it, which is the point: the supervisor should not be able to amplify a
/// fault it cannot understand.
struct Supervisor {
    child: Option<Child>,
    /// Earliest we may spawn again.
    next_spawn: Instant,
    /// Current delay; doubles on each rapid exit, resets after a healthy run.
    backoff: Duration,
    /// When the current child started - distinguishes a crash-loop from someone
    /// simply hanging up after an hour.
    started: Instant,
}

const RESTART_MIN: Duration = Duration::from_millis(500);
const RESTART_MAX: Duration = Duration::from_secs(30);
/// A child that ran at least this long counts as healthy; its exit is not a crash.
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

    /// Restart the child if it exited, or start it if we never managed to.
    fn ensure_running(&mut self, exe: &Path, args: &[String]) {
        if let Some(c) = self.child.as_mut() {
            match c.try_wait() {
                Ok(Some(status)) => {
                    if self.started.elapsed() >= HEALTHY_RUN {
                        self.backoff = RESTART_MIN; // it was fine; not a loop
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
                _ => return, // still running
            }
        }
        if self.child.is_none() && Instant::now() >= self.next_spawn {
            self.child = spawn_or_warn(exe, args);
            self.started = Instant::now();
        }
    }

    /// Stop the child so its binary can be replaced.
    fn stop(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.child = None;
    }

    /// Start immediately after an update - an update is not a crash.
    fn start_now(&mut self, exe: &Path, args: &[String]) {
        self.backoff = RESTART_MIN;
        self.next_spawn = Instant::now();
        self.child = spawn_or_warn(exe, args);
        self.started = Instant::now();
    }
}

/// Keep the child alive for `dur` while we wait to reconnect.
fn supervise(exe: &Path, args: &[String], sup: &mut Supervisor, dur: Duration) {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        sup.ensure_running(exe, args);
        std::thread::sleep(Duration::from_millis(250));
    }
}
