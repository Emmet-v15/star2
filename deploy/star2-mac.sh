#!/usr/bin/env sh
# One-line installer for macOS:
#
#   curl -fsSL https://v15.studio/star2-mac.sh | sh
#
# Installs the runner into ~/star2. The runner fetches the engine itself, so
# there is nothing else to download.
#
# Gatekeeper: these binaries are ad-hoc signed (by the linker). That satisfies
# the arm64 requirement that all code be signed to execute, but it does NOT
# satisfy Gatekeeper - quarantined + ad-hoc is rejected exactly like quarantined
# + unsigned. Only Developer ID + notarization passes that check.
#
# So the thing keeping this install working is that curl never sets
# com.apple.quarantine (only apps opting into LSFileQuarantineEnabled, i.e.
# browsers, do). Download by browser/AirDrop instead and it WILL be blocked.
# The xattr strip below is the recovery for that case.
set -eu

DIR="${STAR2_DIR:-$HOME/star2}"
ARCH="$(uname -m)"

case "$ARCH" in
    arm64)  URL=https://v15.studio/star2-macos ;;
    x86_64)
        echo "Intel Macs are not published yet (arm64 only)." >&2
        echo "If you are on Apple Silicon, run under a native shell, not Rosetta." >&2
        exit 1 ;;
    *) echo "unsupported arch $ARCH" >&2; exit 1 ;;
esac

mkdir -p "$DIR"
echo "==> downloading star2 -> $DIR/star2"
curl -fSL --progress-bar -o "$DIR/star2.part" "$URL"
mv -f "$DIR/star2.part" "$DIR/star2"
chmod +x "$DIR/star2"
xattr -d com.apple.quarantine "$DIR/star2" 2>/dev/null || true

echo
echo "Installed. Run it with your room and name:"
echo "    $DIR/star2 --room myroom --name $(id -un) --stats"
echo
echo "If macOS ever calls it damaged or refuses to verify the developer, the"
echo "binary was quarantined by a browser download. Clear it with:"
echo "    xattr -dr com.apple.quarantine $DIR"
echo
echo "First run will ask for microphone access. The prompt is attributed to your"
echo "terminal app, not to star2 - if you have previously denied it, grant it under"
echo "System Settings > Privacy & Security > Microphone."
