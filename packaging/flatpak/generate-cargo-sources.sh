#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
OUT="$ROOT/packaging/flatpak/cargo-sources.json"
LOCK="$ROOT/src-tauri/Cargo.lock"
GEN_DIR="${TMPDIR:-/tmp}/flatpak-builder-tools-cargo"
GEN="$GEN_DIR/cargo/flatpak-cargo-generator.py"

if [[ ! -f "$LOCK" ]]; then
  echo "Missing Cargo.lock: $LOCK" >&2
  exit 1
fi

if [[ ! -x "$GEN" ]]; then
  rm -rf "$GEN_DIR"
  git clone --depth 1 --filter=blob:none --sparse \
    https://github.com/flatpak/flatpak-builder-tools.git "$GEN_DIR"
  git -C "$GEN_DIR" sparse-checkout set cargo
fi

VENV="$GEN_DIR/.venv"
PYTHON=python3
if python3 - <<'PY' >/dev/null 2>&1
import aiohttp, tomlkit, yaml
PY
then
  PYTHON=python3
elif [[ -x "$VENV/bin/python" ]] && "$VENV/bin/python" - <<'PY' >/dev/null 2>&1
import aiohttp, tomlkit, yaml
PY
then
  PYTHON="$VENV/bin/python"
else
  python3 -m venv "$VENV"
  "$VENV/bin/python" -m pip install --upgrade pip
  "$VENV/bin/python" -m pip install 'aiohttp>=3.9.5,<4.0.0' 'tomlkit>=0.13.3,<1.0' 'PyYAML>=6.0.2,<7.0.0'
  PYTHON="$VENV/bin/python"
fi

"$PYTHON" "$GEN" "$LOCK" -o "$OUT"
# Keep the generated file compatible with the repository's end-of-file pre-commit hook.
python3 - "$OUT" <<'PY'
from pathlib import Path
import sys
path = Path(sys.argv[1])
data = path.read_bytes()
if data and not data.endswith(b"\n"):
    path.write_bytes(data + b"\n")
PY
echo "Wrote $OUT"
