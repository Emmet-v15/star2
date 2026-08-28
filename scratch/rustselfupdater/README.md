# rustselfupdater

The star2 self-update mechanism, lifted out as a standalone crate. One binary
keeps itself current: fetch a manifest, verify the download's SHA-256 against it,
swap the running image by rename, relaunch with the same arguments.

## The flow

1. `check_at_startup` — before your app does anything, compare the running
   image's SHA-256 against the manifest. Stale means: download, verify hash,
   stage beside the binary as `app.new`, then `swap_self`.
2. `watch_for_updates` — optional push channel: connect to a WebSocket, report
   this build's version, and re-run the check whenever the server sends any
   text (star2's server fans out `{"t":"Update"}`). Reconnects every 10 s; a
   30 s silence also triggers one catch-up check.
3. When a check succeeds, the crate latches an internal flag;
   your main loop polls `restart_requested()`, prints whatever it likes,
   and calls `relaunch(&me, &args)`.

## Why rename instead of overwrite

Windows forbids deleting or overwriting a running image but permits renaming
one. So the swap is: running image → `app.old`, staged `app.new` → `app`.
If the second rename fails, `app.old` is renamed back — a failed update leaves
the running build untouched. A leftover `app.old` from a previous run is
removed at startup before checking.

The download is verified **before** anything on disk changes: a bad hash never
reaches the swap.

## Manifest format

```json
{"version":"1.2.3","sha256":"<hex of the exact bytes at url>","url":"https://host/app.exe"}
```

## Usage

Copy the crate in (or vendor `src/lib.rs`) and:

```rust
use rustselfupdater::{check_at_startup, relaunch, restart_requested, watch_for_updates};

fn main() -> anyhow::Result<()> {
    let me = std::env::current_exe()?;
    let args: Vec<String> = std::env::args().skip(1).collect();

    if check_at_startup(&me, "https://host/app.json") {
        return relaunch(&me, &args);          // fresh build takes over, same args
    }

    watch_for_updates(me.clone(), "wss://host/updates",
                      "https://host/app.json", env!("CARGO_PKG_VERSION"));

    loop {
        if restart_requested() { return relaunch(&me, &args); }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}
```

Skip `watch_for_updates` for startup-check-only updating; everything else works
the same. Relaunch always carries the original arguments, so the new build must
never need interactive input to reach the same state.

Not carried over from star2: its restart seed (relay endpoint + peer address
written beside the binary so the successor rejoins faster) — that part is
call-specific and lives in star2.
