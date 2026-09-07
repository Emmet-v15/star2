#!/usr/bin/env bash
# Build star2.exe and publish it to v15.studio.
#
#   ./deploy/publish-client.sh          # build + upload + notify
#   ./deploy/publish-client.sh --check  # ...then verify over HTTPS
#
# Windows only. libopus is compiled from source for the host arch, so there is
# no cross-build shortcut, and Windows is the only platform we ship.
set -euo pipefail

HOST="${STAR2_HOST:-empire}"
WEBROOT=/var/www/v15.studio
REMOTE=star2.exe
MANIFEST=star2.json
LOCAL=target/release/star2.exe

cd "$(dirname "$0")/.."

case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) ;;
    *) echo "publish from Windows - $(uname -s) is not a target we ship" >&2; exit 1 ;;
esac

# Refuse to ship a build that claims to be a version already published. star2
# swaps on hash, so an unbumped version still rolls out - but then the version
# clients report is a lie and "who is stale?" becomes unanswerable. That is
# exactly how everyone sat on 0.1.1 for the whole project.
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
cargo build --release -p star2-app --features star2-app/custom-protocol

echo "==> uploading -> $WEBROOT/$REMOTE"
# Upload to /tmp then move: opc can't write the webroot directly, and a
# partially-transferred file must never be servable.
scp -q "$LOCAL" "$HOST:/tmp/$REMOTE"
ssh "$HOST" "sudo -n mv -f /tmp/$REMOTE $WEBROOT/$REMOTE && sudo -n chmod 644 $WEBROOT/$REMOTE"

# The manifest is what a running star2.exe compares itself against. The hash is
# the integrity check, so it must come from the exact bytes just uploaded.
SHA=$(sha256sum "$LOCAL" | cut -d' ' -f1)
printf '{"version":"%s","sha256":"%s","url":"https://v15.studio/%s"}\n' \
    "$VER" "$SHA" "$REMOTE" > /tmp/star2.json
scp -q /tmp/star2.json "$HOST:/tmp/star2-manifest.json"
ssh "$HOST" "sudo -n mv -f /tmp/star2-manifest.json $WEBROOT/$MANIFEST && sudo -n chmod 644 $WEBROOT/$MANIFEST"
echo "==> manifest $VER $SHA"

# Push to every connected client. This ALWAYS runs - publishing without it
# leaves people on the old build until they happen to reconnect, which is the
# whole failure this is meant to prevent. The token is read off the rendezvous
# server when it isn't already in the environment, so there is no way to
# "forget" it.
TOKEN="${STAR2_TOKEN:-$(ssh "$HOST" "grep '^STAR2_TOKEN=' /home/opc/star2/star2.env | cut -d= -f2")}"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "x-token: $TOKEN" https://star.v15.studio/star2/notify || true)
echo "==> notify: HTTP $code"
[ "$code" = "200" ] || { echo "notify FAILED - clients will not update until they reconnect" >&2; exit 1; }

echo "==> published https://v15.studio/$REMOTE"
if [ "${1:-}" = "--check" ]; then
    curl -s -o /dev/null -w "    http=%{http_code} bytes=%{size_download}\n" \
        "https://v15.studio/$REMOTE"
fi
