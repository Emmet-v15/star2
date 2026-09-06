# Deploying star2

Everything that touches `empire` or `v15.studio` lives here. All scripts are
idempotent and safe to re-run.

The rendezvous box is reached as `ssh empire` — the service lives in
`/home/opc/star2`, config in `/home/opc/star2/star2.env`.

| file | what it does |
|---|---|
| `publish-client.sh` | build `star2.exe`, upload it to `v15.studio/star2.exe`, write the manifest, nudge every connected client |
| `deploy-rendezvous.sh` | cross-compile the rendezvous server to aarch64 and restart it on empire |
| `publish-web.sh` | build the window as a web app and upload it to `star.v15.studio` |
| `star2-rendezvous.service` | the systemd unit as installed on empire |
| `nginx-star2.conf` | the nginx blocks added to v1's vhost |

## Publish a client build

```sh
./deploy/publish-client.sh --check
```

Run it from Windows — `audiopus_sys` compiles libopus from source for the host
arch, so there's no cross-build shortcut for the client, and Windows is the only
platform we ship. The rendezvous server is the opposite: pure Rust, cross-compiles fine.

## Publish the web app

```sh
./deploy/publish-web.sh
```

This is the same `crates/star2-app/ui` the Tauri window loads, built for the
browser. There is no second UI: `engine.svelte.ts` sends its commands to the
Tauri bridge or to `web-engine.svelte.ts`, and `App.svelte` cannot tell which.
The web engine speaks the rendezvous protocol over a WebSocket and carries media
over a WebRTC data channel, framed as the datagram a native peer would have sent.

It lives at the root of <https://star.v15.studio/>, served from
`/var/www/star.v15.studio` by the same vhost that proxies `/star2` — which is
why it needs no configuring: on `https:` it defaults its rendezvous URL to
`wss://star.v15.studio/star2`, its own origin. Nothing restarts, so publishing
never touches a call in progress.

Override the defaults from the URL hash when testing against a local server:
`https://star.v15.studio/#rv=ws://localhost:9101&token=star2-dev`. For UI work
alone, `bun run dev` in `crates/star2-app/ui` serves the same app on :5173.

Two costs worth knowing about:

- The app carries `RENDEZVOUS_TOKEN` — the same one compiled into `star2.exe`,
  so it was already public in every download; this only makes it readable
  without `strings`. Rotating it means changing `main.rs`, `star2.env` and
  `web-engine.svelte.ts` together, and publishing a client build in the same breath.
- `ui/src/lib/room.ts` is a port of star-proto's `room.rs`. It is the one place
  a wire format lives in two languages, and it is there because a browser that
  cannot mint a token can only join guessable rooms. Change both together.

## Deploy the rendezvous server

```sh
./deploy/deploy-rendezvous.sh empire
```

## Ports and why

| | star2 | star v1 (do not disturb) |
|---|---|---|
| UDP | 40001 — reflexive probes only | 40000 — media relay |
| TCP | 9101 behind nginx `/star2` | 9100 behind nginx `/ws` |

Opening a UDP port takes **two** changes, and forgetting the second is the failure
mode that costs an hour:

```sh
# 1. host firewall
sudo firewall-cmd --permanent --add-port=40001/udp && sudo firewall-cmd --reload
# 2. Oracle VCN security list - without this, firewalld is wide open and packets
#    still never reach the box. Verify with: sudo tcpdump -n -i any udp port 40001
```

Both are already applied for 40001/udp. The OCI ingress rule was added with the
`oci` CLI on empire; the previous rule set is backed up at `/tmp/ing.backup.json`.

## CI

`.github/workflows/build.yml` builds the Windows client and the aarch64
rendezvous server on every push to main and uploads both to the `dev-builds`
GitHub release. Publishing a release to v15.studio stays manual:
`./deploy/publish-client.sh`.
