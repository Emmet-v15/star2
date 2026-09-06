# star2

Barebones peer-to-peer voice call. 48 kHz Opus, direct UDP, one small window.

`empire` runs a **rendezvous server**: it brokers the hole punch and
answers reflexive-address probes. Audio never passes through it — once the punch
confirms, packets go straight between the two peers.

## Why 48 kHz and not 44.1 kHz

Opus has no 44.1 kHz mode; libopus accepts 8/12/16/24/48 kHz and runs at 48 kHz
internally. 48 kHz is also the native rate of WASAPI (shared mode), CoreAudio and
Android AAudio, so asking for 44.1 kHz would force a resample on *every* platform
while 48 kHz usually forces none.

The wire format is lossy-but-transparent rather than lossless on purpose. On a UDP
path, packet loss dominates quantisation noise you cannot hear: a lost frame can be
concealed (PLC) or recovered from a copy carried in the next packet, while FLAC would
turn the same loss into an audible hole and make jitter-buffer sizing harder.

Opus' own in-band FEC is not available to us: LBRR is SILK-only and SILK's shortest
frame is 10 ms, so at 5 ms frames Opus is CELT-only and `set_inband_fec` is a silent
no-op. That is why redundancy is done at the packet layer instead — see below.

## Layout

| crate | what |
|---|---|
| `star2-rendezvous` | the server on empire: WebSocket rendezvous + UDP reflexive responder |
| `star2-app`    | the `star2` window: a thin Tauri shell (join, status) plus self-update |
| `guest/`       | a static browser page: joins a room as a WebRTC data-channel guest |

The wire format and the client core are no longer here. They are libraries in
[`star-libs`](../star-libs), a sibling checkout, and star2 depends on them by
path:

| crate | was |
|---|---|
| `star-proto` | `star2-proto` — media header, punch probe, rendezvous JSON, room tokens |
| `star-voice` | `star2-engine` — capture → Opus → UDP → jitter buffer → playback |

What stayed behind is the part that is specific to this deployment: the
rendezvous URL and token, the room the window asks for, the Tauri shell, and
self-update. Cloning star2 now means cloning star-libs beside it.

The jitter buffer and hole-punch FSM are ported from star v1, which is where that
tuning was worked out.

## Building

```sh
cargo build --release -p star2-app        # -> target/release/star2.exe
cargo test --release                      # whole workspace
```

`.github/workflows/build.yml` does this on every push to `main`: the Windows
client on `windows-latest` and the rendezvous server cross-compiled to aarch64
with zigbuild, both uploaded to the `dev-builds` GitHub release. Publishing to
v15.studio stays manual: `./deploy/publish-client.sh`, which refuses to ship
without a version bump in `Cargo.toml` and always POSTs the update nudge.

## Running

Download `star2.exe` from v15.studio once and run it. Type a room token into the
window and hit Join; leave it blank and you get a brand-new room to share. Pick
your input and output devices from the two dropdowns — they default to the system
devices and are remembered between runs. Your display name is the machine's name,
audio is mono 128 kbps.

The window shows status, not history. Everything the engine decides — device
rates, candidates, punch timings, buffer moves — goes to `star2.log` beside the
binary, appended across update restarts and dropped once it passes 4 MB. That is
the file to read after something goes wrong.

There is **one** binary and it updates itself. On startup — and again whenever the
rendezvous server nudges it — it compares its own SHA-256 against the manifest, and if they
differ it downloads the new build and stages it beside the running one.

Windows forbids *deleting or overwriting* a running image but permits *renaming*
one: the current image becomes `star2.old` and the staged build moves into place.
The download is verified before any of that touches disk state that matters, so a
failed update leaves the running build untouched.

Then it restarts into the new build. Idle or mid-call, the path is the same:
stop the engine, relaunch — the restart seed written moments before names the
room, so the fresh build rejoins it without asking — and re-punches through
the paths every cold start already uses. There is no supervising process and no
second binary. An update costs each peer a few seconds of silence and one redial;
the call does not survive the swap by design, and no machinery exists to pretend
otherwise.

To keep that rejoin cheap, the outgoing process leaves a tiny restart seed beside
the binary: the relay's reflex-probe endpoint, the peer's last direct address,
the room token, and a timestamp. The successor uses them only as accelerators —
the reflexive probe fires before signalling finishes, and the hinted endpoint
joins the first candidate wave. Nothing is trusted: every punch still requires
the nonce exchange over the rendezvous server, and a missing or stale (over 120 s)
seed is deleted so plain cold-start discovery proceeds silently.

### How updates arrive

| path | when |
|---|---|
| startup | before the call starts, so a cold launch is never stale |
| WebSocket push | instant — `publish-client.sh` POSTs `/star2/notify`, the server fans out a nudge |
| on reconnect | catches builds shipped while the socket was down |

Push-only; there is no polling loop. Nothing here depends on the rendezvous server being up: if
the manifest or the socket is unreachable, star2 warns and runs the build it has.

The nudge carries no payload beyond "go look again". The manifest and binary are
fetched over TLS and checked against the manifest's SHA-256, so a forged nudge can
at worst cause a wasted re-download. It is **not** a signature: anyone who can
write the webroot can ship a build, which is the same trust boundary as the
download itself.

There is no crash supervisor. If star2 dies, you start it again.

Both peers must join the same room token. The lower session id becomes the punch
controller, so glare can't happen.

The window exposes two controls today: the room token and the input/output device
pickers. Audio is mono 128 kbps at the baked-in rendezvous URL, and your display
name is the machine's name. The engine still supports stereo, bitrate and buffer
tuning; those knobs have not been drawn yet.

## Latency

5 ms Opus frames. The jitter buffer target is `K x p97(jitter) + margin`, clamped —
so it tracks the link instead of hoarding a fixed delay. The output pre-buffer grows
on device under-run and shrinks after a clean stretch, with the clean period doubling
each time it flaps so a marginal device settles instead of oscillating.

If capture granularity is coarse (packets leave in bursts), receivers read that as
jitter and size the buffer up; a smaller device buffer usually shrinks the far
end's buffer with it.

The buffer only ever grows or sheds in bulk. It never discards a frame to trim
itself — deliberately dropping good audio to save a few milliseconds is a glitch you
can hear, and the latency it buys back is not worth it.

## Loss

Each packet can carry a copy of the previous frame's Opus payload. When a packet
goes missing, the gap is filled from its successor's copy instead of being concealed,
which costs one extra frame of nothing (the successor was already in the buffer).

It is negotiated, not always on: a receiver sets `RED_WANTED` on its own outgoing
packets while it is seeing loss and for 10 s after, and a sender only doubles up for
a peer that asked. A clean link pays nothing.

## Deploying the rendezvous server

```sh
./deploy/deploy-rendezvous.sh empire
```

On empire the service is `star2-rendezvous.service`, config in `~/star2/star2.env`.
It is deliberately separate from star v1's `star-relay.service` (UDP 40000 / TCP
9100), which it does not touch.

| | star2 | star v1 |
|---|---|---|
| UDP | 40001 (reflexive probes only) | 40000 (media relay) |
| TCP | 9101 behind nginx `/star2` | 9100 behind nginx `/ws` |

Both the firewalld rule and the **OCI VCN security list** ingress rule for
`40001/udp` are already in place. The OCI rule is the one that is easy to forget:
firewalld can be wide open and packets will still never reach the box without it.

## Status

Verified:
- Two clients, punch confirmed in 60–160 ms, audio both ways, 0% loss.
- Two clients on one host against a local rendezvous server, punch confirmed in
  87/163 ms, media both ways at 200 pkt/s, 0% loss, the full latency chain logged —
  re-confirmed on the handover-free build.
- Reflexive discovery through the real NAT (`wss://` rendezvous + UDP 40001).
- unit tests covering: wire round-trips, punch authorisation and the retry/timeout role
  split, a full two-state punch over real loopback sockets, restart re-adoption
  (a fresh controller re-offers to the waiting peer; a fresh responder arms
  silently), jitter estimator, resampler, room tokens, restart seeds,
  packet redundancy.

Not yet verified:
- A punch between two *different* networks. Both test peers shared a LAN candidate,
  so the reflexive path has been discovered but not yet traversed end to end.
- The update restart end to end across a real publish (stop → swap → relaunch →
  same room → re-punch). Its pieces are covered: argument stability across
  relaunch, stage-and-verify swap, and the re-adoption tests above.

## Known gaps

- **Windows only.** macOS and Android are out of scope until there is a plan that
  isn't a hand-rolled toolchain per platform. (The engine no longer *requires*
  Windows at the source level: the socket-duplication handover was the last
  `std::os::windows` user and is gone.)
- **Media never falls back through the rendezvous server, by design.** A failed punch ends the call. Symmetric NAT and
  CGNAT (mobile data especially) are the cases that will fail.
- **An update interrupts the call.** Both peers drop; each relaunches into the new
  build and rejoins the room automatically. Seconds of silence, no manual steps -
  that is the accepted cost of the deleted generational-handover machinery.
- A failed punch no longer ends the attempt. The controller aborts, backs off (5 s
  doubling to a 60 s ceiling), and re-offers while both peers stay in the room;
  from the third failure on it says so honestly ("likely symmetric NAT or CGNAT").
  What it still never does is relay through the rendezvous server — if the punch
  cannot succeed, neither can the call.
- 1:1 only — a third peer in a room ends the call rather than mixing.
- The window has no stereo / bitrate / buffer controls yet; the engine takes all
  of those, the UI just does not expose them.
- No encryption of the media payload. The punch nonce is protected by the
  rendezvous TLS, so an off-path attacker cannot redirect media, but an on-path one
  can read audio.
