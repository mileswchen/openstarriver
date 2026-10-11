#!/bin/bash
# Starriver Desktop — one-shot build & deploy script
# Usage: ./scripts/setup.sh [debug|release]
set -euo pipefail

MODE="${1:-debug}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DESKTOP="$ROOT/desktop"

echo "=== Starriver Desktop Setup ($MODE) ==="

# ── 1. Build Rust napi-rs backend ──
echo ""
echo "[1/4] Building Rust backend..."
cd "$ROOT"
if [ "$MODE" = "release" ]; then
    cargo build -p starriver-desktop --release
    DYLIB="$ROOT/target/release/libstarriver_desktop.dylib"
    NODE_FILE="$ROOT/target/release/starriver_desktop.node"
else
    cargo build -p starriver-desktop
    DYLIB="$ROOT/target/debug/libstarriver_desktop.dylib"
    NODE_FILE="$ROOT/target/debug/starriver_desktop.node"
fi

if [ ! -f "$DYLIB" ]; then
    echo "ERROR: dylib not found at $DYLIB"
    exit 1
fi
echo "  Rust build OK ($DYLIB)"

# ── 2. Copy dylib → .node (Electron-loadable format) ──
echo ""
echo "[2/4] Copying dylib → .node..."
cp "$DYLIB" "$NODE_FILE"
echo "  $NODE_FILE"

# ── 3. Install npm dependencies ──
echo ""
echo "[3/4] Installing npm dependencies (Electron ~150MB, may take a few minutes)..."
cd "$DESKTOP/electron"
if [ -d "node_modules/electron" ]; then echo "  node_modules exists, verifying..."; npm install --prefer-offline; else echo "  Downloading..."; npm install; fi
echo ""; echo "  npm install done"

# ── 4. Verify ──
echo ""
echo "[4/4] Verifying..."
echo "  Rust dylib: $(ls -lh "$DYLIB" | awk '{print $5}')"
echo "  Node file:  $(ls -lh "$NODE_FILE" | awk '{print $5}')"
echo "  Electron:   $(npx electron --version 2>/dev/null || echo 'not found')"
echo ""
echo "=== Setup complete ==="
echo ""
echo "Start Electron: cd desktop/electron && npm start"
echo "Start Bun GUI:  cd desktop && bun run packages/gui/server.ts"
