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

```sh
cargo build --release
./target/release/star2 --url wss://star.v15.studio/star2 --token <TOKEN> --room general --name me
```

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
| `--stats` | off | 1 Hz `rtt / jitter / buf / loss / out / rx / play` readout |
| `--list-devices` | | print devices and exit |

`--stats` is the diagnostic tool: `rx` and `play` should both sit at 200/s. A gap
between them, or either falling short of 200, says the fault is local (a starved
thread) rather than the network.

## Path QoS

Media is marked DSCP **EF** (46) so WMM access points put it in the voice class,
which attacks local queueing jitter — and jitter is what sets our buffer depth.

The implementation is split because `setsockopt(IP_TOS)` is **silently ignored on
Windows** (since XP SP2). Unix marks the socket once at bind; Windows must use
qWAVE, which needs a destination address, so it can only mark once the punch has
confirmed where the peer is. A `[qos]` line reports which happened.

This only affects the first and last hop. ISPs rewrite or ignore DSCP at the edge,
so it does nothing for transit jitter — for a long path, bufferbloat (SQM/`cake`)
on both routers is the far bigger lever.

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
