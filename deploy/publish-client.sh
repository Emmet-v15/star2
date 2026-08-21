#!/usr/bin/env bash
# Build the client for THIS host's platform and publish it to v15.studio.
#
#   ./deploy/publish-client.sh          # build + upload
#   ./deploy/publish-client.sh --check  # build + upload, then verify over HTTPS
#
# Naming matches star v1's convention (v15.studio/star.exe|apk|dmg):
#   Windows -> star2.exe      macOS -> star2-macos      Linux -> star2-linux
#
# Run this from the platform you are publishing for - libopus is compiled from
# source for the host arch, so there is no cross-build shortcut for the client.
set -euo pipefail

HOST="${STAR2_HOST:-empire}"
WEBROOT=/var/www/v15.studio

cd "$(dirname "$0")/.."

# LOCAL/REMOTE is the ENGINE - the auto-updating part the manifest tracks.
# RUN_LOCAL/RUN_REMOTE is the runner, which users download once by hand.
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*)
        LOCAL=target/release/star2-engine.exe; REMOTE=star2-engine.exe
        RUN_LOCAL=target/release/star2.exe;    RUN_REMOTE=star2.exe
        MANIFEST=star2.json ;;
    Darwin)
        LOCAL=target/release/star2-engine;     REMOTE=star2-engine-macos
        RUN_LOCAL=target/release/star2;        RUN_REMOTE=star2-macos
        MANIFEST=star2-macos.json ;;
    Linux)
        LOCAL=target/release/star2-engine;     REMOTE=star2-engine-linux
        RUN_LOCAL=target/release/star2;        RUN_REMOTE=star2-linux
        MANIFEST=star2-linux.json ;;
    *) echo "unknown platform $(uname -s)" >&2; exit 1 ;;
esac

# Refuse to ship a build that claims to be a version already published. The
# updater swaps on hash, so an unbumped version still rolls out - but then the
# version clients report is a lie and "who is stale?" becomes unanswerable.
# That is exactly how everyone sat on 0.1.1 for the whole project.
VER=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([0-9]+\.[0-9]+\.[0-9]+)".*/\1/')
LIVE=$(curl -fsS --max-time 10 "https://v15.studio/$MANIFEST" 2>/dev/null \
       | sed -E 's/.*"version":"([^"]*)".*/\1/' || true)
if [ -n "$LIVE" ] && [ "$LIVE" = "$VER" ]; then
    echo "refusing to publish: $MANIFEST is already version $VER" >&2
    echo "bump 'version' in the workspace Cargo.toml first" >&2
    exit 1
fi
echo "==> version $LIVE -> $VER"

echo "==> building $LOCAL"
cargo build --release -p star2-cli -p star2-updater

echo "==> uploading -> $WEBROOT/$REMOTE"
# Upload to /tmp then move: opc can't write the webroot directly, and a
# partially-transferred file must never be servable.
scp -q "$LOCAL" "$HOST:/tmp/$REMOTE"
ssh "$HOST" "sudo -n mv -f /tmp/$REMOTE $WEBROOT/$REMOTE && sudo -n chmod 644 $WEBROOT/$REMOTE"

# Ship the runner too. It self-replaces (rename the running exe aside, drop the
# new one in its place, relaunch), so the manifest carries its hash as well.
RUN_SHA=""
if [ -f "$RUN_LOCAL" ]; then
    scp -q "$RUN_LOCAL" "$HOST:/tmp/$RUN_REMOTE"
    ssh "$HOST" "sudo -n mv -f /tmp/$RUN_REMOTE $WEBROOT/$RUN_REMOTE && sudo -n chmod 644 $WEBROOT/$RUN_REMOTE"
    RUN_SHA=$(sha256sum "$RUN_LOCAL" | cut -d' ' -f1)
    echo "==> published https://v15.studio/$RUN_REMOTE (runner)"
fi

# Manifest: what the updater compares against. The hash is the integrity check,
# so it must be computed from the exact bytes that were uploaded.
SHA=$(sha256sum "$LOCAL" | cut -d' ' -f1)
printf '{"version":"%s","sha256":"%s","url":"https://v15.studio/%s","runner_sha256":"%s","runner_url":"https://v15.studio/%s"}\n' \
    "$VER" "$SHA" "$REMOTE" "$RUN_SHA" "$RUN_REMOTE" > /tmp/star2.json
scp -q /tmp/star2.json "$HOST:/tmp/star2-manifest.json"
ssh "$HOST" "sudo -n mv -f /tmp/star2-manifest.json $WEBROOT/$MANIFEST && sudo -n chmod 644 $WEBROOT/$MANIFEST"
echo "==> manifest $VER $SHA"

# The macOS one-line installer. Uploaded from every platform so it can't drift
# out of sync with the repo copy - it's 1KB, the re-upload costs nothing.
scp -q deploy/star2-mac.sh "$HOST:/tmp/star2-mac.sh"
ssh "$HOST" "sudo -n mv -f /tmp/star2-mac.sh $WEBROOT/star2-mac.sh && sudo -n chmod 644 $WEBROOT/star2-mac.sh"

# Nudge every connected updater to re-check. Non-fatal: the updater polls anyway,
# so a failed notify only delays the rollout, it doesn't break it.
if [ -n "${STAR2_TOKEN:-}" ]; then
    code=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
        -H "x-token: $STAR2_TOKEN" https://star.v15.studio/star2/notify || true)
    echo "==> notify: HTTP $code"
else
    echo "==> notify skipped (set STAR2_TOKEN to push instantly)"
fi

echo "==> published https://v15.studio/$REMOTE"
if [ "${1:-}" = "--check" ]; then
    curl -s -o /dev/null -w "    http=%{http_code} bytes=%{size_download}\n" \
        "https://v15.studio/$REMOTE"
fi
