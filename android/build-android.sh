#!/usr/bin/env bash
#
# build-android.sh — Full native Android build for TollGate.
#
# Produces a debug APK at android/app/build/outputs/apk/debug/.
#
# Prerequisites:
#   * Rust + cargo-ndk + Android targets (aarch64, armv7, x86_64)
#   * Android SDK (NDK 27.x) with build-tools 35/36
#   * JDK 17+
#
# Usage:
#   ./build-android.sh           # debug APK
#   ./build-android.sh release   # release APK

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ANDROID_DIR="$REPO_ROOT/android"
BUILD_TYPE="${1:-debug}"
NDK_VERSION="${NDK_VERSION:-27.2.12479018}"

export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$HOME/Android/Sdk/ndk/$NDK_VERSION}"
export ANDROID_NDK_ROOT="$ANDROID_NDK_HOME"

# --- 1. Build native .so for all ABIs ---
echo "==> Building Rust core for Android (3 ABIs)..."
cd "$REPO_ROOT"
cargo ndk \
    -t arm64-v8a \
    -t armeabi-v7a \
    -t x86_64 \
    build -p tollgate-android ${BUILD_TYPE:+--$BUILD_TYPE}

# --- 2. Copy .so files to jniLibs ---
echo "==> Copying native libraries to jniLibs..."
# cargo-ndk emits to target/<rust-triple>/, not target/<abi>/ — map ABI → triple.
triple_for_abi() {
    case "$1" in
        arm64-v8a)    echo "aarch64-linux-android" ;;
        armeabi-v7a)  echo "armv7-linux-androideabi" ;;
        x86_64)       echo "x86_64-linux-android" ;;
        *) echo "error: unknown ABI '$1'" >&2; exit 1 ;;
    esac
}
JNI_LIBS="$ANDROID_DIR/app/src/main/jniLibs"
for abi in arm64-v8a armeabi-v7a x86_64; do
    triple="$(triple_for_abi "$abi")"
    mkdir -p "$JNI_LIBS/$abi"
    cp "$REPO_ROOT/target/$triple/$BUILD_TYPE/libtollgate_android.so" \
       "$JNI_LIBS/$abi/"
    echo "    $abi ($triple): $(ls -lh "$JNI_LIBS/$abi/libtollgate_android.so" | awk '{print $5}')"
done

# --- 3. Generate UniFFI Kotlin bindings ---
echo "==> Generating UniFFI Kotlin bindings..."
# Use the arm64 .so as the template for binding generation
cargo run -p tollgate-android --bin uniffi-bindgen -- \
    generate \
    --library "$REPO_ROOT/target/aarch64-linux-android/$BUILD_TYPE/libtollgate_android.so" \
    --language kotlin \
    --out-dir "$ANDROID_DIR/kotlin"
echo "    Generated: $(ls "$ANDROID_DIR/kotlin/")"

# --- 4. Build the APK ---
echo "==> Building $BUILD_TYPE APK via Gradle..."
cd "$ANDROID_DIR"
./gradlew "assemble${BUILD_TYPE^}" --no-daemon

APK="$ANDROID_DIR/app/build/outputs/apk/$BUILD_TYPE/app-$BUILD_TYPE.apk"
if [[ -f "$APK" ]]; then
    echo ""
    echo "✅ APK built: $APK"
    echo "   Size: $(du -h "$APK" | cut -f1)"
else
    echo "❌ APK not found at expected path"
    exit 1
fi
