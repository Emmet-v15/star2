# star2

Barebones peer-to-peer voice call. 48 kHz Opus, direct UDP, no UI.

`empire` runs a **signalling-only** rendezvous server: it brokers the hole punch and
answers reflexive-address probes. Audio never passes through it — once the punch
confirms, packets go straight between the two peers.

## Why 48 kHz and not 44.1 kHz

Opus has no 44.1 kHz mode; libopus accepts 8/12/16/24/48 kHz and runs at 48 kHz
internally. 48 kHz is also the native rate of WASAPI (shared mode), CoreAudio and
Android AAudio, so asking for 44.1 kHz would force a resample on *every* platform
while 48 kHz usually forces none.

The wire format is lossy-but-transparent rather than lossless on purpose. On a UDP
path, packet loss dominates quantisation noise you cannot hear: Opus can conceal a
lost frame (PLC) and rebuild it from the next one (in-band FEC), while FLAC would
turn the same loss into an audible hole and make jitter-buffer sizing harder.

## Layout

| crate | what |
|---|---|
| `star2-proto`  | wire types: media header, punch probe, signalling JSON |
| `star2-signal` | the server on empire: WebSocket rendezvous + UDP reflexive responder |
| `star2-engine` | client core: capture → Opus → UDP → jitter buffer → playback |
| `star2-cli`    | the `star2` binary; the CLI is the whole UI |

The jitter buffer and hole-punch FSM are ported from star v1, which is where that
tuning was worked out.

## Running

Download `star2.exe` from v15.studio once and run it. It fetches the engine itself:

```sh
star2.exe --room general --name me
```

`star2` is the **runner**: it verifies the engine's SHA-256 against the manifest,
downloads it if stale, then launches and supervises it. Every flag is passed
straight through. `star2-engine` is the actual voice client and can be run
directly — you just don't get auto-update.

The split exists because Windows won't overwrite a running image, so something has
to outlive the engine to replace it. It also means the part that changes often
(the engine) is the part that auto-updates, while the supervisor rarely moves.

### How updates arrive

| path | when |
|---|---|
| WebSocket push | instant — `publish-client.sh` POSTs `/star2/notify`, the server fans out a nudge |
| on connect | once per connection, catching builds shipped while the runner was offline |

Push-only; there is no polling loop. A dropped socket reconnects, and the check on
reconnect covers anything missed while it was down.

The nudge carries no payload beyond "go look again". The manifest and binary are
fetched over TLS and checked against the manifest's SHA-256, so a forged nudge can
at worst cause a wasted re-download. It is **not** a signature: anyone who can
write the webroot can ship a build, which is the same trust boundary as the
download itself.

A missing or corrupt engine simply fails the hash comparison and is reinstalled,
so the runner repairs itself rather than bricking.

Both peers must pass the same `--room`. The lower session id becomes the punch
controller, so glare can't happen.

| flag | default | meaning |
|---|---|---|
| `--url` | `ws://127.0.0.1:9101` | signal server |
| `--room` | `general` | both peers must match |
| `--name` | hostname | display name |
| `--token` | `star2-dev` | shared secret |
| `--stereo` | off | send 2 channels instead of 1 |
| `--bitrate` | 128k mono / 256k stereo | Opus bitrate |
| `--input` / `--output` | system default | device name substring |
| `--dev-buf <MS>` | 0 (device default) | device buffer request; lower = less latency |
| `--list-devices` | | print devices and exit |

## Latency

5 ms Opus frames. The jitter buffer target is `K x p97(jitter) + margin`, clamped —
so it tracks the link instead of hoarding a fixed delay. The output pre-buffer grows
on device under-run and shrinks after a clean stretch, with the clean period doubling
each time it flaps so a marginal device settles instead of oscillating.

If capture granularity is coarse (packets leave in bursts), receivers read that as
jitter and size the buffer up. `--dev-buf 5` asks for a smaller device buffer and
usually shrinks the far end's buffer with it.

## Deploying the signal server

```sh
./deploy/deploy-signal.sh empire
```

On empire the service is `star2-signal.service`, config in `~/star2/star2.env`.
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
- Reflexive discovery through the real NAT (`wss://` signalling + UDP 40001).
- 19 unit tests: wire round-trips, punch authorisation, jitter estimator, resampler.

Not yet verified:
- A punch between two *different* networks. Both test peers shared a LAN candidate,
  so the reflexive path has been discovered but not yet traversed end to end.
- macOS and Android. CI builds macOS artifacts; Android is not started.

## Known gaps

- **No relay fallback, by design.** A failed punch ends the call. Symmetric NAT and
  CGNAT (mobile data especially) are the cases that will fail.
- 1:1 only — a third peer in a room ends the call rather than mixing.
- No encryption of the media payload. The punch nonce is protected by the
  signalling TLS, so an off-path attacker cannot redirect media, but an on-path one
  can read audio.
