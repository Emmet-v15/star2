#!/usr/bin/env bash
# Cross-compile the rendezvous server and deploy it to the rendezvous box.
#
# The rendezvous server is pure Rust, so this needs only cargo-zigbuild + zig - no
# toolchain on the server (there isn't one). Re-running is safe: it replaces the
# binary and restarts the unit, leaving star v1's relay alone.
set -euo pipefail

HOST="${1:-empire}"
TARGET=aarch64-unknown-linux-gnu.2.34
BIN=target/aarch64-unknown-linux-gnu/release/star2-rendezvous

cd "$(dirname "$0")/.."

echo "==> building for $TARGET"
cargo zigbuild -p star2-rendezvous --release --target "$TARGET"

echo "==> shipping to $HOST"
ssh "$HOST" 'mkdir -p ~/star2'
# Copy beside the live binary then move into place: a partially-written file can
# never be what systemd restarts into.
scp -q "$BIN" "$HOST:~/star2/star2-rendezvous.new"
ssh "$HOST" 'mv ~/star2/star2-rendezvous.new ~/star2/star2-rendezvous && chmod +x ~/star2/star2-rendezvous'

echo "==> restarting"
ssh "$HOST" 'sudo -n systemctl restart star2-rendezvous && sleep 1 && systemctl is-active star2-rendezvous'

echo "==> recent logs"
ssh "$HOST" 'journalctl -u star2-rendezvous -n 5 --no-pager'
