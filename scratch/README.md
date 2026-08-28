# scratch

Satellites of star2 that were living in a loose `~/projects/scratch` folder with
no version control. Folded in here so they stop being one `rm -rf` from gone.
Excluded from the workspace (`exclude = ["scratch"]`): they build on their own,
and neither belongs in a `cargo test --workspace` run.

| directory | what it is |
|---|---|
| `rustselfupdater` | star2's self-update flow lifted out as a standalone crate — manifest fetch, SHA-256 verify, rename-swap, relaunch. `crates/star2-app/src/updater.rs` is the version that ships. |
| `star2-demo-rig` | a two-peer local loopback rig: serve `star2.json` over `http://127.0.0.1:8123`, point one build at it, watch it update itself. |

## What was deliberately not folded in

The original folder was 734 MB, almost none of it source:

- `rustselfupdater/target/` — 706 MB of build output.
- `star2-demo-rig/{star2,peerB}.exe` — 13 MB each, rebuildable from this tree.
- `star2-demo-rig/{server,tone}.log` — run output from a specific afternoon.

The manifest kept in `star2-demo-rig/star2.json` still names the SHA-256 of the
0.4.5 `star2.exe` that is no longer here, so regenerate it against whatever
binary you actually serve.
