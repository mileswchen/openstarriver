#!/bin/bash
# Starriver Desktop — 启动 Bun SSR GUI (浏览器访问)
# 不需要 Electron，直接浏览器打开 http://localhost:5173
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DESKTOP="$ROOT/desktop"

cd "$DESKTOP"

# 检查 Bun
if ! command -v bun &> /dev/null; then
    echo "ERROR: bun not found. Install: curl -fsSL https://bun.sh/install | bash"
    exit 1
fi

# 安装依赖 (如果需要)
if [ ! -d "node_modules" ]; then
    echo "Installing bun dependencies..."
    bun install --silent
fi

# 检查 .node 文件
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
