#!/usr/bin/env bash
# Builds THIRD_PARTY_NOTICES.txt with the licenses of everything shipped with the app:
# pdfium (and its components), the JavaScript libraries and the Rust ones.
# Requires: cargo install cargo-about --locked --features cli
# Usage: scripts/generate-notices.sh   (after pnpm install)
set -euo pipefail
cd "$(dirname "$0")/.."
out=THIRD_PARTY_NOTICES.txt
rust=$(mktemp)
(cd src-tauri && cargo about generate licenses/about.hbs -o "$rust")

{
  echo "Pliego incluye software de terceros. Estos son sus avisos de licencia."
  echo "El código propio de Pliego se distribuye bajo la licencia GPL-3.0 (ver LICENSE)."
  echo "El código fuente de todas las librerías de Rust está disponible en https://crates.io."
  echo
  echo "================================================================================"
  echo "PDFium (motor de PDF) — https://pdfium.googlesource.com/pdfium"
  echo "Compilado por pdfium-binaries — https://github.com/bblanchon/pdfium-binaries"
  echo "================================================================================"
  for f in licenses/pdfium/*; do
    echo
    echo "--------------------------------------------------------------------------------"
    echo "$(basename "${f%.*}")"
    echo
    cat "$f"
  done
  echo
  echo "================================================================================"
  echo "Librerías de JavaScript"
  echo "================================================================================"
  for pkg in @tauri-apps/api @tauri-apps/plugin-dialog; do
    echo
    echo "--------------------------------------------------------------------------------"
    echo "$pkg $(node -p "require('./node_modules/$pkg/package.json').version")"
    echo
    # Both are published as "MIT OR Apache-2.0" (Tauri Programme); the plugin doesn't ship the
    # license text, so the MIT one from @tauri-apps/api is used.
    cat node_modules/@tauri-apps/api/LICENSE_MIT
  done
  echo
  echo "================================================================================"
  cat "$rust"
} > "$out"
rm "$rust"
echo "Listo: $out"
