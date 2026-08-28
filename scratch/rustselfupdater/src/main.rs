use std::time::Duration;

use anyhow::Context;
use rustselfupdater::{check_at_startup, relaunch, restart_requested, watch_for_updates};

const MANIFEST_URL: &str = "https://example.com/app.json";
const UPDATES_WS: &str = "wss://example.com/updates";

fn main() -> anyhow::Result<()> {
    let me = std::env::current_exe().context("locate the running binary")?;
    let args: Vec<String> = std::env::args().skip(1).collect();

    if check_at_startup(&me, MANIFEST_URL) {
        return relaunch(&me, &args);
    }

    println!("demo {} running", env!("CARGO_PKG_VERSION"));
    watch_for_updates(me.clone(), UPDATES_WS, MANIFEST_URL, env!("CARGO_PKG_VERSION"));

    loop {
        if restart_requested() {
            println!("update restarting");
            return relaunch(&me, &args);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
