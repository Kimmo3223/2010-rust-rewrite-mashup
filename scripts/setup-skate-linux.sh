#!/usr/bin/env bash
# Linux counterpart of the Windows first-run Skate 3 setup.
#
#   scripts/setup-skate-linux.sh "/path/to/Skate 3/default.xex"
#
# Converts the Skate 3 files skating needs from your own extracted Xbox 360
# copy (default.xex with its data folder beside it) into ./skate-data, then
# records IW4L_SKATE_ASSETS in ./.env. Runs the same converter the Windows
# release bundles, from the SK8-ENGINE/skate-3-rust-engine tools at the
# revision skate/crates was taken from. Nothing from the game is uploaded.
#
# Needs: git, python3 (with venv), rustc. Run it again to redo the conversion.
set -euo pipefail

ENGINE_URL=https://github.com/SK8-ENGINE/skate-3-rust-engine
ENGINE_REV=cb79689

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
ENGINE="$ROOT/iw4l-artifacts/skate-engine"
OUT="$ROOT/skate-data"

die() { echo "error: $*" >&2; exit 1; }

[ $# -eq 1 ] || die "usage: $0 /path/to/default.xex"
XEX=$(realpath "$1") || die "cannot find $1"
[ "$(basename "$XEX" | tr '[:upper:]' '[:lower:]')" = default.xex ] ||
    die "select default.xex from an extracted Skate 3 folder (ISO files do not work)"
case "$ROOT" in *" "*) die "the repo path contains a space, which make's .env include cannot handle: $ROOT";; esac
for tool in git python3 rustc; do
    command -v "$tool" >/dev/null || die "$tool is not installed"
done

echo "==> Skate engine tools ($ENGINE_REV)"
if [ ! -d "$ENGINE/.git" ]; then
    mkdir -p "$(dirname "$ENGINE")"
    git clone --quiet "$ENGINE_URL" "$ENGINE"
fi
git -C "$ENGINE" fetch --quiet origin || true
git -C "$ENGINE" checkout --quiet "$ENGINE_REV"

echo "==> Python environment"
if [ ! -x "$ENGINE/.venv/bin/python" ]; then
    python3 -m venv "$ENGINE/.venv" ||
        die "python3 venv failed (Debian/Ubuntu: sudo apt install python3-venv)"
fi
"$ENGINE/.venv/bin/python" -m pip install --quiet --upgrade pip
"$ENGINE/.venv/bin/python" -m pip install --quiet 'numpy>=2.2,<3' 'Pillow>=11.3'

echo "==> Native RefPack decoder"
# fast_refpack.py looks for this exact file name; ctypes loads the ELF shared
# object regardless of the .dll extension. Without it the tools fall back to a
# much slower pure-Python decoder.
mkdir -p "$ENGINE/target/native"
rustc --edition 2024 --crate-type cdylib -C opt-level=3 -C panic=abort \
    "$ENGINE/tools/asset_pipeline/refpack_native.rs" \
    -o "$ENGINE/target/native/refpack.dll"

echo "==> Converting Skate 3 data from $XEX"
cp "$ROOT/skate/converter/iw4l_skate_convert.py" "$ENGINE/iw4l_skate_convert.py"
"$ENGINE/.venv/bin/python" "$ENGINE/iw4l_skate_convert.py" --xex "$XEX" --out "$OUT"

ASSETS="$OUT/assets"
for needed in private/skater.glb private/game.json \
    private/stock/physics-skeletons.json private/stock/skater-collections.json; do
    [ -f "$ASSETS/$needed" ] || die "conversion finished without $needed"
done

echo "==> Recording IW4L_SKATE_ASSETS in .env"
ENV_FILE="$ROOT/.env"
touch "$ENV_FILE"
# Unquoted on purpose: the Makefile includes .env as make syntax, where quotes
# would become part of the value.
grep -v -E '^[[:space:]]*(IW4L_SKATE_ASSETS|IW4L_SKATE)[[:space:]]*=' "$ENV_FILE" > "$ENV_FILE.tmp" || true
echo "IW4L_SKATE_ASSETS=$ASSETS" >> "$ENV_FILE.tmp"
mv "$ENV_FILE.tmp" "$ENV_FILE"

echo
echo "Skate 3 data ready. Plug in a controller, run 'make menu' or 'make map mp_rust', and press J."
