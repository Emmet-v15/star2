#!/usr/bin/env bash
# Build the window as a web app and publish it to https://star.v15.studio/.
#
# Same Svelte app the Tauri shell loads - only the engine underneath differs,
# and it picks itself (ui/src/lib/engine.svelte.ts). Static files only: nothing
# restarts, so publishing cannot disturb a call in progress. Safe to re-run.
set -euo pipefail

HOST="${1:-empire}"
DIR=/var/www/star.v15.studio

cd "$(dirname "$0")/.."

echo "==> building ui"
(cd crates/star2-app/ui && bun run build)

# Vite hashes asset filenames, so a copy would leave every previous build's
# assets lying around. Stage the whole tree and swap it in one mv instead.
echo "==> uploading to $HOST:$DIR"
tar -C crates/star2-app/ui/dist -cz . | ssh "$HOST" "set -e
  rm -rf /tmp/star2-web && mkdir -p /tmp/star2-web
  tar -C /tmp/star2-web -xz
  sudo -n chown -R nginx:nginx /tmp/star2-web
  sudo -n rm -rf $DIR.old
  if [ -d $DIR ]; then sudo -n mv $DIR $DIR.old; fi
  sudo -n mv /tmp/star2-web $DIR
  sudo -n rm -rf $DIR.old"

echo "==> live"
curl -fsS -o /dev/null -w "  %{http_code}  %{url_effective}\n" https://star.v15.studio/
curl -fsS https://star.v15.studio/ | grep -o 'assets/[^"]*\.js' | while read -r a; do
  curl -fsS -o /dev/null -w "  %{http_code}  %{url_effective}\n" "https://star.v15.studio/$a"
done
