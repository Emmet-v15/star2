# star2 — working principles

These are hard constraints, not preferences. When a change conflicts with one of
them, the change is wrong.

## 1. MVP. Do not overbuild.

Build the simplest thing that actually works, and stop. No speculative
generality, no config knobs "in case", no abstraction with one implementation,
no framework where a function will do.

If you catch yourself adding machinery to handle a case that has not happened,
delete it. A feature that is not needed today is a feature that is wrong
tomorrow.

Prefer deleting code to adding it. A change that removes lines and keeps the
behaviour is a good change.

## 2. Backwards compatibility is not a goal.

Break the wire format. Break the CLI. Break the file layout. Rename things.
Delete the old path outright rather than keeping it alive behind a flag.

There is no install base to protect: everyone re-downloads from v15.studio, and
the client auto-updates. Compatibility shims are pure cost. If dropping
compatibility makes the code simpler or more correct, drop it and do not leave a
migration path behind.

Corollary: no version gating, no feature negotiation, no "if peer is old" branch,
unless the alternative genuinely does not work.

## 3. Simple beats clever, and simple beats complete.

One code path is better than two that share a helper. One binary is better than
two that talk to each other. One way to do a thing.

When two mechanisms do nearly the same job, collapse them into one even if the
merged version is slightly worse at the edges.

## 4. Scope

- P2P voice, 1:1, low latency. That is the product.
- **Automatic self-update is a headline feature, not plumbing.** See §8.
- **Rendezvous only** — the rendezvous server brokers the hole punch and answers
  reflexive probes. Media never passes through it and **never falls back through
  it.** A failed punch ends the call.
- **Must not depend on the rendezvous server being up.** A call already in
  progress survives the rendezvous server going away.
- **Minimal window, terminal restraint.** One Tauri window: room token, join,
  status. Nothing gets a button until it earns one.
- Windows is the target. macOS and Android are out of scope until there is a
  plan that does not cost more than the feature is worth.

## 5. Audio

- 48 kHz, Opus, 5 ms frames. These are settled; do not re-litigate them.
- Never deliberately corrupt the stream. Do not drop, splice, or discard a good
  frame to hit a latency target. If the buffer needs to shrink, it waits for a
  gap that is already there.
- Default is mono 128 kbps. Stereo is opt-in.

## 6. Code style

- **No comments in `.rs` files.** Name things so the comment is unnecessary. If
  a decision needs prose, it goes in a commit message or in this file.
- Shell scripts may and should have comments.
- Tests assert on behaviour that a user would notice, and their failure messages
  say what broke.

## 7. Build and release

- Every build script lives in `deploy/`. There is no second place to look.
- Every build is uploaded to v15.studio.
- **Publishing is never gated on who is mid-call.** Ship the update; live peers
  take it without being asked - a mid-call client stops, swaps, relaunches, and
  rejoins the same room (§8).
- Never `taskkill //F //IM star2.exe //T` — it kills every peer's client too.
  Always target a PID.

## 8. Updates are automatic, delivered by restart-and-rejoin

The rendezvous server says a new build exists; the client is running it moments
later. No prompt, no restart the user has to perform. **The call does not survive
the swap** - that promise was retired with the generational-handoff architecture.
What the update promises instead: the client stages and verifies the download,
stops the engine, renames the new binary into place, and relaunches; the fresh
build rejoins the same room token and re-punches through the exact paths every
cold start already uses. An update costs a peer seconds of silence and one
redial, never a manual step and never a stuck client.

These follow from it, and are settled:

- **Stage and verify before touching the running image.** A download that fails
  its hash check leaves the current build running untouched.
- **Relaunch carries the room.** A relaunched build must never sit at a prompt
  asking what to join: the restart seed written just before the swap names the
  room, and the fresh build rejoins it without asking.
- **Reconnect paths are the update surface.** Rendezvous reconnect, roster
  re-evaluation, and the punch FSM with its retry/backoff are what make a restart
  cheap; treat their health as update health.
- **A restart seed is a hint, never a shortcut.** The outgoing build may leave the
  relay's reflex-probe endpoint and the peer's last direct address beside the
  binary; the successor fires its reflexive probe early and merges the hint into
  its candidate list. Nonces still gate every probe, and a missing or stale seed
  is discarded silently.
- **One window, no chrome.** The Tauri shell inherited the terminal's restraint
  (§4); the window is not an invitation to start adding chrome.
- **One binary.** No supervisor, no parent process. The runner/engine split was
  tried and deleted (`477c190`); the socket-duplication handover built on top of
  it was deleted with the restart model - do not reintroduce either.
