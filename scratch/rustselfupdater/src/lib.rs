use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const RECONNECT_DELAY: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_secs(30);

static RESTART: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub sha256: String,
    pub url: String,
}

pub fn restart_requested() -> bool {
    RESTART.load(Ordering::Relaxed)
}

pub fn block_on<F: Future>(f: F) -> F::Output {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a tokio runtime")
        .block_on(f)
}

pub async fn fetch_manifest(manifest_url: &str) -> Result<Manifest> {
    let txt = reqwest::get(manifest_url).await?.error_for_status()?.text().await?;
    serde_json::from_str(&txt).context("parse manifest")
}

pub async fn stage(me: &Path, url: &str, sha: &str) -> Result<PathBuf> {
    let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;
    let got = hex(Sha256::digest(&bytes).as_slice());
    if !got.eq_ignore_ascii_case(sha) {
        bail!("sha256 mismatch (manifest {sha}, got {got})");
    }
    let staged = me.with_extension("new");
    std::fs::write(&staged, &bytes).context("write the new binary")?;
    Ok(staged)
}

pub fn swap_self(me: &Path, staged: &Path) -> Result<()> {
    let old = me.with_extension("old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(me, &old).context("move the running binary aside")?;
    if let Err(e) = std::fs::rename(staged, me) {
        let _ = std::fs::rename(&old, me);
        return Err(e).context("move the new binary into place");
    }
    Ok(())
}

pub fn is_stale(path: &Path, want_sha256: &str) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => !hex(Sha256::digest(&bytes).as_slice()).eq_ignore_ascii_case(want_sha256),
        Err(_) => true,
    }
}

pub async fn check_for_update(me: &Path, manifest_url: &str) -> bool {
    let manifest = match fetch_manifest(manifest_url).await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("update unavailable: {e}");
            return false;
        }
    };
    if !is_stale(me, &manifest.sha256) {
        return false;
    }

    println!("update {} downloading", manifest.version);
    let staged = match stage(me, &manifest.url, &manifest.sha256).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("update download failed: {e}");
            return false;
        }
    };
    if let Err(e) = swap_self(me, &staged) {
        eprintln!("update install failed: {e}");
        let _ = std::fs::remove_file(&staged);
        return false;
    }
    println!("update {} installed", manifest.version);
    true
}

pub fn check_at_startup(me: &Path, manifest_url: &str) -> bool {
    let _ = std::fs::remove_file(me.with_extension("old"));
    block_on(check_for_update(me, manifest_url))
}

pub fn relaunch(me: &Path, args: &[String]) -> Result<()> {
    Command::new(me)
        .args(args)
        .spawn()
        .context("relaunch into the new build - start it again by hand")?;
    Ok(())
}

pub fn watch_for_updates(
    me: PathBuf,
    updates_ws: &'static str,
    manifest_url: &'static str,
    current_version: &'static str,
) {
    std::thread::spawn(move || {
        block_on(async {
            loop {
                if let Err(e) = watch_session(&me, updates_ws, manifest_url, current_version).await {
                    eprintln!("update channel: {e}");
                }
                if RESTART.load(Ordering::Relaxed) {
                    return;
                }
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        })
    });
}

async fn watch_session(
    me: &Path,
    updates_ws: &str,
    manifest_url: &str,
    current_version: &str,
) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (ws, _) = tokio_tungstenite::connect_async(updates_ws)
        .await
        .context("connect to update channel")?;
    let (mut tx, mut rx) = ws.split();
    let _ = tx.send(Message::Text(current_version.into())).await;

    if check_for_update(me, manifest_url).await {
        RESTART.store(true, Ordering::Relaxed);
        return Ok(());
    }
    while let Ok(Some(msg)) = tokio::time::timeout(QUIET, rx.next()).await {
        msg?;
        if check_for_update(me, manifest_url).await {
            RESTART.store(true, Ordering::Relaxed);
            return Ok(());
        }
    }
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_binary_counts_as_stale() {
        assert!(is_stale(Path::new("definitely/not/here.exe"), "ab"));
    }

    #[test]
    fn matching_hash_is_current_and_anything_else_is_stale() {
        let dir = std::env::temp_dir().join(format!("rustselfupdater-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let me = dir.join("app.exe");
        std::fs::write(&me, b"payload").unwrap();

        assert!(!is_stale(&me, &hex(Sha256::digest(b"payload").as_slice())));
        assert!(is_stale(&me, "00"));

        let _ = std::fs::remove_file(&me);
    }
}
