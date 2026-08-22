#!/usr/bin/env bash
# Cross-compile the signal server and deploy it to the signalling box.
#
# The signal server is pure Rust, so this needs only cargo-zigbuild + zig - no
# toolchain on the server (there isn't one). Re-running is safe: it replaces the
# binary and restarts the unit, leaving star v1's relay alone.
set -euo pipefail

HOST="${1:-empire}"
TARGET=aarch64-unknown-linux-gnu.2.34
BIN=target/aarch64-unknown-linux-gnu/release/star2-signal

cd "$(dirname "$0")/.."

echo "==> building for $TARGET"
cargo zigbuild -p star2-signal --release --target "$TARGET"

echo "==> shipping to $HOST"
ssh "$HOST" 'mkdir -p ~/star2'
# Copy beside the live binary then move into place: a partially-written file can
# never be what systemd restarts into.
scp -q "$BIN" "$HOST:~/star2/star2-signal.new"
ssh "$HOST" 'mv ~/star2/star2-signal.new ~/star2/star2-signal && chmod +x ~/star2/star2-signal'

echo "==> restarting"
ssh "$HOST" 'sudo -n systemctl restart star2-signal && sleep 1 && systemctl is-active star2-signal'

echo "==> recent logs"
ssh "$HOST" 'journalctl -u star2-signal -n 5 --no-pager'
