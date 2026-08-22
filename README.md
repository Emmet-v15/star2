# star2

Barebones peer-to-peer voice call. 48 kHz Opus, direct UDP, no UI.

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
| `star2-proto`  | wire types: media header, punch probe, rendezvous JSON |
| `star2-rendezvous` | the server on empire: WebSocket rendezvous + UDP reflexive responder |
| `star2-engine` | client core: capture → Opus → UDP → jitter buffer → playback |
| `star2-cli`    | the `star2` binary: CLI, self-update. The CLI is the whole UI |

The jitter buffer and hole-punch FSM are ported from star v1, which is where that
tuning was worked out.

## Running

Download `star2.exe` from v15.studio once and run it. Run it with no arguments and
it asks for a room token; blank makes a new one.

```sh
star2.exe --room general --name me
```

There is **one** binary and it updates itself. On startup — and again whenever the
rendezvous server nudges it — it compares its own SHA-256 against the manifest, and if they
differ it downloads the new build, renames itself to `star2.old` and moves the new
one into place.

Windows forbids *deleting or overwriting* a running image but permits *renaming*
one, which is what makes that work without a second supervising process.

Idle, it then relaunches with the same arguments. **Mid-call it hands the call
over instead**, and the call does not drop. The old process spawns the new one and
duplicates the live UDP socket into it with `WSADuplicateSocket`, so the local port
and the NAT mapping survive and the peer keeps talking to the same address.

Duplicating a socket does not surrender it, so until the new build says it is
carrying traffic the old one can still take the call back; a new build that dies on
startup leaves the call running. The two directions cut over in opposite orders,
because a shared UDP endpoint splits reads rather than copying them: **receive**
stops on the old before it starts on the new, and the kernel socket buffer covers
the gap, while **send** starts on the new before it stops on the old, because a
duplicate packet is cheap and a hole in the stream is not.

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

Both peers must pass the same `--room`. The lower session id becomes the punch
controller, so glare can't happen.

| flag | default | meaning |
|---|---|---|
| `--url` | `wss://star.v15.studio/star2` | rendezvous server |
| `--room` | prompted | both peers must match |
| `--create <NAME>` | | mint a new room token, print it, join it |
| `--name` | hostname | display name |
| `--token` | baked in | shared secret |
| `--stereo` | off | send 2 channels instead of 1 |
| `--bitrate` | 128k mono / 256k stereo | Opus bitrate |
| `--input` / `--output` | system default | device name substring |
| `--dev-buf <MS>` | 0 (device default) | device buffer request; lower = less latency |
| `--stats` | off | 1 Hz jitter/loss/rate readout |
| `--verbose` | off | audio/punching/jitter internals |
| `--list-devices` | | print devices and exit |

`--create` mints a **new** token every run; use `--room` to return to an existing one.

## Latency

5 ms Opus frames. The jitter buffer target is `K x p97(jitter) + margin`, clamped —
so it tracks the link instead of hoarding a fixed delay. The output pre-buffer grows
on device under-run and shrinks after a clean stretch, with the clean period doubling
each time it flaps so a marginal device settles instead of oscillating.

If capture granularity is coarse (packets leave in bursts), receivers read that as
jitter and size the buffer up. `--dev-buf 5` asks for a smaller device buffer and
usually shrinks the far end's buffer with it.

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
- Reflexive discovery through the real NAT (`wss://` rendezvous + UDP 40001).
- A mid-call update across a real publish, both peers: the call survives, the peer
  keeps receiving on the same address, and the jitter buffer holds at 20 ms through
  the cutover.
- 63 unit tests: wire round-trips, punch authorisation, jitter estimator, resampler,
  room tokens, argument resolution, packet redundancy, socket handover including a
  cutover across a real process boundary.

Not yet verified:
- A punch between two *different* networks. Both test peers shared a LAN candidate,
  so the reflexive path has been discovered but not yet traversed end to end.

## Known gaps

- **Windows only.** macOS and Android are out of scope until there is a plan that
  isn't a hand-rolled toolchain per platform.
- **Media never falls back through the rendezvous server, by design.** A failed punch ends the call. Symmetric NAT and
  CGNAT (mobile data especially) are the cases that will fail.
- **A handover still costs one peer about 0.4 s of degraded playout**, spread over
  the two seconds around the cutover, when both peers update at once — which is the
  normal case, since one nudge reaches both. The other peer hears roughly a frame.
  The buffer no longer blows out and nothing resyncs, but §8 asks for neither side
  hearing it at all, and this is not that yet. The asymmetry is unexplained.
- A failed punch is fatal rather than dropping back to idle to wait for the peer.
- 1:1 only — a third peer in a room ends the call rather than mixing.
- No encryption of the media payload. The punch nonce is protected by the
  rendezvous TLS, so an off-path attacker cannot redirect media, but an on-path one
  can read audio.
