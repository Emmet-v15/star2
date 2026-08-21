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

case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) LOCAL=target/release/star2.exe; REMOTE=star2.exe ;;
    Darwin)               LOCAL=target/release/star2;     REMOTE=star2-macos ;;
    Linux)                LOCAL=target/release/star2;     REMOTE=star2-linux ;;
    *) echo "unknown platform $(uname -s)" >&2; exit 1 ;;
esac

echo "==> building $LOCAL"
cargo build --release -p star2-cli

echo "==> uploading -> $WEBROOT/$REMOTE"
# Upload to /tmp then move: opc can't write the webroot directly, and a
# partially-transferred file must never be servable.
scp -q "$LOCAL" "$HOST:/tmp/$REMOTE"
ssh "$HOST" "sudo -n mv -f /tmp/$REMOTE $WEBROOT/$REMOTE && sudo -n chmod 644 $WEBROOT/$REMOTE"

echo "==> published https://v15.studio/$REMOTE"
if [ "${1:-}" = "--check" ]; then
    curl -s -o /dev/null -w "    http=%{http_code} bytes=%{size_download}\n" \
        "https://v15.studio/$REMOTE"
fi
