#!/usr/bin/env bash
# Downloads libpdfium.so for Android into the app's jniLibs (not committed to git).
# The version must match the one pdfium-render expects (feature pdfium_7881), like the desktop build.
set -euo pipefail

RELEASE="chromium%2F7881"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
JNI="$ROOT/src-tauri/gen/android/app/src/main/jniLibs"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# pdfium-binaries name -> Android ABI folder
for pair in "arm64:arm64-v8a" "arm:armeabi-v7a" "x64:x86_64"; do
  name="${pair%%:*}"
  abi="${pair##*:}"
  mkdir -p "$TMP/$name" "$JNI/$abi"
  curl -fsSL -o "$TMP/$name.tgz" \
    "https://github.com/bblanchon/pdfium-binaries/releases/download/$RELEASE/pdfium-android-$name.tgz"
  tar xzf "$TMP/$name.tgz" -C "$TMP/$name"
  cp "$TMP/$name/lib/libpdfium.so" "$JNI/$abi/libpdfium.so"
  echo "$abi: listo"
done
