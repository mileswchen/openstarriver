#!/bin/bash
# Starriver Desktop — 启动 Electron 窗口
# 需要先运行 setup.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ELECTRON_DIR="$ROOT/desktop/electron"

# 检查 .node 文件
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
# tsc 会把 preload.js 重写为带相对 require 的输出(沙箱化 preload 加载不了),
# 因此 tsc 之后必须按 npm run build 的顺序把 renderer 与 preload 都重新打包。
npx tsc
npx esbuild renderer/app.tsx --bundle --outfile=renderer/bundle.js --platform=browser --format=iife 2>&1 | tail -3
npx esbuild preload.ts --bundle --platform=node --format=cjs --external:electron --outfile=preload.js
echo ""
echo "Starting Electron..."
npx electron .
