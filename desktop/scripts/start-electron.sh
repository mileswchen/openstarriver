#!/bin/bash
# Starriver Desktop — launch the Electron window
# Requires setup.sh to run first
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ELECTRON_DIR="$ROOT/desktop/electron"

# Check the .node file
for path in "$ROOT/target/debug/starriver_desktop.node" "$ROOT/target/release/starriver_desktop.node"; do
    if [ -f "$path" ]; then
        echo "Found: $path"
        break
    fi
done
if [ ! -f "$ROOT/target/debug/starriver_desktop.node" ] && [ ! -f "$ROOT/target/release/starriver_desktop.node" ]; then
    echo "ERROR: .node file not found. Run setup.sh first."
    exit 1
fi

cd "$ELECTRON_DIR"
echo "Compiling TypeScript..."
# tsc rewrites preload.js with relative requires (a sandboxed preload cannot load them),
# so after tsc both renderer and preload must be re-bundled, in npm run build order.
npx tsc
npx esbuild renderer/app.tsx --bundle --outfile=renderer/bundle.js --platform=browser --format=iife 2>&1 | tail -3
npx esbuild preload.ts --bundle --platform=node --format=cjs --external:electron --outfile=preload.js
echo ""
echo "Starting Electron..."
npx electron .
