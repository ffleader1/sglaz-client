#!/usr/bin/env bash
#
# Cross-compile the sglaz agent for the three targets we ship:
#
#   linux-amd64    x86_64-unknown-linux-musl   (static, runs on any Linux)
#   windows-amd64  x86_64-pc-windows-gnu       (sglaz.exe)
#   darwin-arm64   aarch64-apple-darwin        (Apple Silicon)
#
# Artifacts land in ./dist/ named to match the server's release target keys
# (linux-amd64, windows-amd64, darwin-arm64), so you can upload them straight to
# a GitHub Release / the Releases page.
#
# Builder auto-detection (override with BUILDER=zig|cross|cargo):
#   zig    cargo-zigbuild + zig      → best on macOS, no C toolchains needed
#   cross  cargo cross               → Docker-based, no local toolchains needed
#   cargo  plain cargo               → needs native cross-linkers installed
#
# Usage:
#   ./build-release.sh                 # build all three
#   ./build-release.sh linux-amd64     # build just one (or several) by key
#   BUILDER=cross ./build-release.sh   # force a builder
#
set -euo pipefail
cd "$(dirname "$0")"

BIN="sglaz"
OUT="dist"

# Map: <release-key> = <rust-target>
declare -a KEYS=(linux-amd64 windows-amd64 darwin-arm64)
target_for() {
  case "$1" in
    linux-amd64)   echo "x86_64-unknown-linux-musl" ;;
    windows-amd64) echo "x86_64-pc-windows-gnu" ;;
    darwin-arm64)  echo "aarch64-apple-darwin" ;;
    *) echo "" ;;
  esac
}

# Which keys to build (all, or the ones passed as args).
WANT=("$@")
if [ ${#WANT[@]} -eq 0 ]; then WANT=("${KEYS[@]}"); fi

# --- pick a builder ---
detect_builder() {
  if [ -n "${BUILDER:-}" ]; then echo "$BUILDER"; return; fi
  if command -v cargo-zigbuild >/dev/null 2>&1 && command -v zig >/dev/null 2>&1; then
    echo zig; return
  fi
  if command -v cross >/dev/null 2>&1; then echo cross; return; fi
  echo cargo
}
BUILDER="$(detect_builder)"

echo "==> sglaz cross-build (builder: $BUILDER)"
case "$BUILDER" in
  zig)   BUILD_CMD=(cargo zigbuild) ;;
  cross) BUILD_CMD=(cross build) ;;
  cargo) BUILD_CMD=(cargo build)
         echo "    note: plain cargo needs native cross-linkers installed:"
         echo "          linux-musl → musl-cross,  windows-gnu → mingw-w64" ;;
  *) echo "unknown BUILDER=$BUILDER (use zig|cross|cargo)"; exit 1 ;;
esac

# Helpful hint if the preferred tooling is missing.
if [ "$BUILDER" = cargo ] && ! command -v cargo-zigbuild >/dev/null 2>&1; then
  echo "    tip: for painless macOS cross-builds:"
  echo "          brew install zig && cargo install cargo-zigbuild"
fi

mkdir -p "$OUT"

for key in "${WANT[@]}"; do
  triple="$(target_for "$key")"
  if [ -z "$triple" ]; then
    echo "!! unknown target key: $key (valid: ${KEYS[*]})"; exit 1
  fi

  echo "==> $key  ($triple)"
  rustup target add "$triple" >/dev/null 2>&1 || true

  "${BUILD_CMD[@]}" --release --target "$triple"

  # Locate the produced binary and copy it out with a friendly name.
  src="target/$triple/release/$BIN"
  dst="$OUT/$BIN-$key"
  if [[ "$key" == windows-* ]]; then
    src="$src.exe"; dst="$dst.exe"
  fi
  if [ ! -f "$src" ]; then
    echo "!! expected artifact not found: $src"; exit 1
  fi
  cp "$src" "$dst"
  echo "    -> $dst"
done

echo
echo "==> done. artifacts in ./$OUT:"
ls -lh "$OUT"
