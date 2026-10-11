#!/bin/bash
# Starriver Desktop — launch the Bun SSR GUI (browser access)
# No Electron needed — open http://localhost:5173 in a browser
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DESKTOP="$ROOT/desktop"

cd "$DESKTOP"

# Check Bun
if ! command -v bun &> /dev/null; then
    echo "ERROR: bun not found. Install: curl -fsSL https://bun.sh/install | bash"
    exit 1
fi

# Install dependencies (if needed)
if [ ! -d "node_modules" ]; then
    echo "Installing bun dependencies..."
    bun install --silent
fi

# Check the .node file
NODE_FILE=""
for path in "$ROOT/target/debug/starriver_desktop.node" "$ROOT/target/release/starriver_desktop.node"; do
    if [ -f "$path" ]; then
        NODE_FILE="$path"
        break
    fi
done
if [ -z "$NODE_FILE" ]; then
    echo "ERROR: .node file not found. Run setup.sh first."
    exit 1
fi

echo "Starriver Desktop GUI → http://localhost:5173"
echo "Pages: / (Hub)  /chat  /skills  /settings"
echo ""
exec bun run packages/gui/server.ts
