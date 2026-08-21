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
        RUN_LOCAL=target/release/star2.exe;    RUN_REMOTE=star2.exe ;;
    Darwin)
        LOCAL=target/release/star2-engine;     REMOTE=star2-engine-macos
        RUN_LOCAL=target/release/star2;        RUN_REMOTE=star2-macos ;;
    Linux)
        LOCAL=target/release/star2-engine;     REMOTE=star2-engine-linux
        RUN_LOCAL=target/release/star2;        RUN_REMOTE=star2-linux ;;
    *) echo "unknown platform $(uname -s)" >&2; exit 1 ;;
esac

echo "==> building $LOCAL"
cargo build --release -p star2-cli -p star2-updater

echo "==> uploading -> $WEBROOT/$REMOTE"
# Upload to /tmp then move: opc can't write the webroot directly, and a
# partially-transferred file must never be servable.
scp -q "$LOCAL" "$HOST:/tmp/$REMOTE"
ssh "$HOST" "sudo -n mv -f /tmp/$REMOTE $WEBROOT/$REMOTE && sudo -n chmod 644 $WEBROOT/$REMOTE"

# Ship the runner too. It does NOT self-update (it's the thing holding the file
# handle during a swap), so users re-download it by hand on the rare occasions it
# changes - but publishing it every time keeps the download current.
if [ -f "$RUN_LOCAL" ]; then
    scp -q "$RUN_LOCAL" "$HOST:/tmp/$RUN_REMOTE"
    ssh "$HOST" "sudo -n mv -f /tmp/$RUN_REMOTE $WEBROOT/$RUN_REMOTE && sudo -n chmod 644 $WEBROOT/$RUN_REMOTE"
    echo "==> published https://v15.studio/$RUN_REMOTE (runner)"
fi

# Manifest: what the updater compares against. The hash is the integrity check,
# so it must be computed from the exact bytes that were uploaded.
SHA=$(sha256sum "$LOCAL" | cut -d' ' -f1)
VER=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([0-9]+\.[0-9]+\.[0-9]+)".*/\1/')
printf '{"version":"%s","sha256":"%s","url":"https://v15.studio/%s"}\n' \
    "$VER" "$SHA" "$REMOTE" > /tmp/star2.json
scp -q /tmp/star2.json "$HOST:/tmp/star2-manifest.json"
ssh "$HOST" "sudo -n mv -f /tmp/star2-manifest.json $WEBROOT/star2.json && sudo -n chmod 644 $WEBROOT/star2.json"
echo "==> manifest $VER $SHA"

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
