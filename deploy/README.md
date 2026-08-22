# Deploying star2

Everything that touches `empire` or `v15.studio` lives here. All scripts are
idempotent and safe to re-run.

| file | what it does |
|---|---|
| `publish-client.sh` | build `star2.exe`, upload it to `v15.studio/star2.exe`, write the manifest, nudge every connected client |
| `deploy-rendezvous.sh` | cross-compile the rendezvous server to aarch64 and restart it on empire |
| `star2-rendezvous.service` | the systemd unit as installed on empire |
| `nginx-star2.conf` | the nginx location block added to v1's vhost |

## Publish a client build

```sh
./deploy/publish-client.sh --check
```

Run it from Windows — `audiopus_sys` compiles libopus from source for the host
arch, so there's no cross-build shortcut for the client, and Windows is the only
platform we ship. The rendezvous server is the opposite: pure Rust, cross-compiles fine.

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

`.github/workflows/build.yml` builds the Windows client plus the aarch64
rendezvous server. **Artifact upload currently fails**: the account's Actions artifact
storage quota is full (star v1's CI uploads an ~80 MB AppImage and an APK on every
run). Until that's cleared, publish with `publish-client.sh` — which is also why
that script exists.
