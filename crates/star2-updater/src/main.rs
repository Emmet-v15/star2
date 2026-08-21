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
use std::time::Duration;

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
    let mut child = spawn_or_warn(&exe, &args);

    loop {
        // Connect, then serve nudges until the socket dies. A check runs on connect
        // too, which covers the case where a build landed while we were offline.
        if let Err(e) = run_session(&exe, &args, &mut child).await {
            eprintln!("  updater: {e}");
        }
        // Supervise across the reconnect gap so a dead child isn't left dead.
        supervise(&exe, &args, &mut child, RECONNECT_DELAY);
    }
}

/// Hold the update socket, checking on connect and on every nudge, while keeping
/// the child alive. Returns when the socket closes.
async fn run_session(exe: &Path, args: &[String], child: &mut Option<Child>) -> Result<()> {
    let (ws, _) = tokio_tungstenite::connect_async(UPDATES_WS)
        .await
        .context("connect to update channel")?;
    println!("  update channel connected");
    let (_tx, mut rx) = ws.split();

    check_and_apply(exe, args, child).await;

    // Interval, not sleep: `select!` cancels the losing branches on every iteration,
    // so a `sleep` here would be restarted each time round and never fire.
    let mut live = tokio::time::interval(Duration::from_secs(2));
    live.tick().await; // the first tick completes immediately - discard it

    loop {
        tokio::select! {
            msg = rx.next() => match msg {
                Some(Ok(_)) => {
                    println!("  update announced");
                    check_and_apply(exe, args, child).await;
                }
                _ => return Ok(()), // closed or errored; caller reconnects
            },
            // Process supervision only - restart the child if it died on its own.
            // This never touches the network.
            _ = live.tick() => ensure_running(exe, args, child),
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

/// Download and install `m`, reporting rather than propagating failure.
async fn install(exe: &Path, m: &Manifest) {
    println!("  installing build {}", m.version);
    match update(exe, m).await {
        Ok(()) => println!("  now on {}", m.version),
        Err(e) => eprintln!("  update failed, keeping current build: {e}"),
    }
}

/// The push path: hash differs -> stop the engine -> swap it -> start it again on
/// the same arguments, so it rejoins the same room.
///
/// Stop before swapping. Windows would allow renaming the running image out of the
/// way, but killing first means the file is untouched by anyone when we replace it,
/// which is one less thing to be subtle about.
async fn check_and_apply(exe: &Path, args: &[String], child: &mut Option<Child>) {
    let Some(manifest) = stale_manifest(exe).await else { return };
    if let Some(c) = child.as_mut() {
        println!("  stopping engine for update");
        let _ = c.kill();
        let _ = c.wait();
    }
    *child = None;
    install(exe, &manifest).await;
    // Same `args` the user launched us with - so it rejoins the same room.
    *child = spawn_or_warn(exe, args);
}

/// Download, verify, and move into place. The download lands beside the target so
/// the final move is same-volume (and therefore atomic).
async fn update(exe: &Path, m: &Manifest) -> Result<()> {
    let bytes = reqwest::get(&m.url).await?.error_for_status()?.bytes().await?;

    let got = hex(Sha256::digest(&bytes).as_slice());
    if !got.eq_ignore_ascii_case(&m.sha256) {
        // Refusing here is the whole point of the manifest: a truncated or
        // tampered download must never be executed.
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

    // Windows won't overwrite a running image, but it will rename one. Move the
    // old aside rather than deleting it, so a failed swap is recoverable.
    let old = exe.with_extension("old");
    let _ = std::fs::remove_file(&old);
    if exe.exists() {
        std::fs::rename(exe, &old).context("move old binary aside")?;
    }
    if let Err(e) = std::fs::rename(&tmp, exe) {
        let _ = std::fs::rename(&old, exe); // put it back
        return Err(e).context("install new binary");
    }
    let _ = std::fs::remove_file(&old);
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

/// Restart the child if it exited, or start it if we never managed to.
fn ensure_running(exe: &Path, args: &[String], child: &mut Option<Child>) {
    match child {
        Some(c) => {
            if let Ok(Some(status)) = c.try_wait() {
                println!("  star2-engine exited ({status}) - restarting");
                *child = spawn_or_warn(exe, args);
            }
        }
        None => *child = spawn_or_warn(exe, args),
    }
}

/// Keep the child alive for `dur` while we wait to reconnect.
fn supervise(exe: &Path, args: &[String], child: &mut Option<Child>, dur: Duration) {
    let deadline = std::time::Instant::now() + dur;
    while std::time::Instant::now() < deadline {
        ensure_running(exe, args, child);
        std::thread::sleep(Duration::from_millis(500));
    }
}
