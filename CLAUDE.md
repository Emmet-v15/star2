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
- **Seamless self-update is a headline feature, not plumbing.** See §8.
- **Rendezvous only** — the rendezvous server brokers the hole punch and answers
  reflexive probes. Media never passes through it and **never falls back through
  it.** A failed punch ends the call.
- **Must not depend on the rendezvous server being up.** A call already in
  progress survives the rendezvous server going away.
- No UI beyond the terminal.
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
  take it mid-call, without being asked and without dropping the call (§8).
- Never `taskkill //F //IM star2.exe //T` — it kills every peer's client too.
  Always target a PID.

## 8. Updates are seamless, including mid-call

The rendezvous server says a new build exists; the client is running it moments
later. No prompt, no restart the user has to perform, and **no dropped call** — a peer
mid-conversation upgrades without either side hearing it happen. This is a goal
of the project in its own right, not a detail of the build pipeline. Cost that
buys seamlessness is worth paying; §1 does not apply to it.

The mechanism is generational handoff. The old process spawns the new one and
duplicates its live UDP socket into it with `WSADuplicateSocket`, so the local
port and the NAT mapping survive the swap and the peer sees nothing at all.

These follow from it, and are settled:

- **The old build must survive a failed handover.** Duplicating a socket does
  not surrender it. A new build that dies on startup leaves the call running.
  Never tear the old one down before the new one is carrying traffic.
- **Carry the media sequence counter across.** A new build that restarts it at
  zero stalls the peer's jitter buffer. This bug is already in the history once.
- **Overlap rather than gap.** During cutover, briefly let both builds send
  instead of neither. A duplicate packet is cheap; a hole in the stream is what
  §5 forbids.
- **One binary.** The handoff is between generations, not managed by a parent, so
  a supervisor buys nothing. The runner/engine split was tried and deleted
  (`477c190`); do not reintroduce it to solve this.
- **Terminal, not GUI.** A console is inherited across the handoff. A window
  would have to be recreated, which is visible. A GUI must justify itself
  against that cost, not just against §4.
