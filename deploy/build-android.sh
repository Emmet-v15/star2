#!/usr/bin/env bash
# Build the Android client (arm64-v8a) and optionally publish the APK.
#
#   ./deploy/build-android.sh            # build target/android/star2.apk
#   ./deploy/build-android.sh --publish  # build, then upload to v15.studio/star2.apk
#   ./deploy/build-android.sh --install  # build, then adb install on the attached device
#
# Unlike the desktop client there is no auto-update here: Android will not let
# an app replace its own APK without being device owner, so the phone is
# updated by re-downloading v15.studio/star2.apk by hand.
set -euo pipefail

HOST="${STAR2_HOST:-empire}"
WEBROOT=/var/www/v15.studio
ABI=arm64-v8a
TRIPLE=aarch64-linux-android
API=26

cd "$(dirname "$0")/.."
ROOT=$(pwd)

# --- toolchain discovery ------------------------------------------------
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
[ -n "$SDK" ] || { echo "set ANDROID_HOME to your Android SDK" >&2; exit 1; }
SDK=$(echo "$SDK" | tr '\\' '/')

NDK="${ANDROID_NDK_ROOT:-}"
if [ -z "$NDK" ]; then
    NDK=$(ls -d "$SDK"/ndk/* 2>/dev/null | sort -V | tail -1 || true)
fi
[ -n "$NDK" ] || { echo "no NDK found under $SDK/ndk" >&2; exit 1; }
NDK=$(echo "$NDK" | tr '\\' '/')
echo "==> ndk $NDK"

command -v cargo-ndk >/dev/null || { echo "run: cargo install cargo-ndk" >&2; exit 1; }

# Gradle is not on PATH; Android Studio keeps it in the wrapper cache.
GRADLE="${GRADLE_BIN:-$(ls -d ~/.gradle/wrapper/dists/gradle-*-bin/*/gradle-*/bin/gradle 2>/dev/null | sort -V | tail -1 || true)}"
[ -n "$GRADLE" ] || { echo "no gradle found; set GRADLE_BIN" >&2; exit 1; }
echo "==> gradle $GRADLE"

# --- libopus ------------------------------------------------------------
# audiopus_sys cannot cross-build Opus: the copy it vendors is autotools-only,
# and on a Windows host its build script falls back to a prebuilt MSVC .lib.
# So build Opus ourselves against the NDK and hand the crate the result.
OPUS_VER=1.5.2
OPUS_SRC="$ROOT/target/android/opus-$OPUS_VER"
OPUS_BUILD="$ROOT/target/android/build-$ABI"

if [ ! -f "$OPUS_BUILD/libopus.a" ]; then
    if [ ! -d "$OPUS_SRC" ]; then
        echo "==> fetching libopus $OPUS_VER"
        mkdir -p "$ROOT/target/android"
        curl -fsSL --max-time 180 \
            "https://downloads.xiph.org/releases/opus/opus-$OPUS_VER.tar.gz" \
            -o "$ROOT/target/android/opus.tar.gz"
        tar xzf "$ROOT/target/android/opus.tar.gz" -C "$ROOT/target/android"
    fi
    echo "==> building libopus for $ABI"
    cmake -S "$OPUS_SRC" -B "$OPUS_BUILD" -G Ninja \
        -DCMAKE_TOOLCHAIN_FILE="$NDK/build/cmake/android.toolchain.cmake" \
        -DANDROID_ABI="$ABI" -DANDROID_PLATFORM="android-$API" \
        -DCMAKE_BUILD_TYPE=Release -DOPUS_BUILD_SHARED_LIBRARY=OFF \
        -DOPUS_BUILD_TESTING=OFF -DOPUS_BUILD_PROGRAMS=OFF >/dev/null
    cmake --build "$OPUS_BUILD" --config Release >/dev/null
fi
echo "==> libopus $OPUS_BUILD/libopus.a"

# --- rust cdylib --------------------------------------------------------
export OPUS_LIB_DIR="$OPUS_BUILD"
export OPUS_STATIC=1
export OPUS_NO_PKG=1

# audiopus_sys emits no `rerun-if-env-changed`, so cargo happily reuses a build
# script run from before OPUS_LIB_DIR was set - and silently links the MSVC lib
# instead. Dropping its cache is the only way to make the env var stick.
rm -rf "$ROOT/target/$TRIPLE"/*/build/audiopus_sys-*

echo "==> building libstar2_android.so"
cargo ndk -t "$ABI" -P "$API" -o android/app/src/main/jniLibs \
    build --release -p star2-android

SO="$ROOT/android/app/src/main/jniLibs/$ABI/libstar2_android.so"
[ -f "$SO" ] || { echo "cdylib was not produced" >&2; exit 1; }

# --- apk ----------------------------------------------------------------
VER=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([0-9]+\.[0-9]+\.[0-9]+)".*/\1/')
echo "==> assembling apk $VER"
( cd android && "$GRADLE" assembleRelease --console=plain -q )

APK=$(ls "$ROOT/android/app/build/outputs/apk/release"/*.apk | head -1)
cp "$APK" "$ROOT/target/android/star2.apk"
echo "==> built $ROOT/target/android/star2.apk"

case "${1:-}" in
    --install)
        adb install -r "$ROOT/target/android/star2.apk"
        ;;
    --publish)
        scp -q "$ROOT/target/android/star2.apk" "$HOST:/tmp/star2.apk"
        ssh "$HOST" "sudo -n mv -f /tmp/star2.apk $WEBROOT/star2.apk && sudo -n chmod 644 $WEBROOT/star2.apk"
        echo "==> published https://v15.studio/star2.apk"
        ;;
esac
