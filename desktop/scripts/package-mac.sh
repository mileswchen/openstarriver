#!/bin/bash
# Starriver Desktop — Mac 安装包打包脚本
# 输出: desktop/electron/dist/Starriver-*.dmg
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ELECTRON_DIR="$ROOT/desktop/electron"
DIST_DIR="$ELECTRON_DIR/dist"
RELEASE="$ROOT/target/release"

echo "=== Starriver Desktop — Mac DMG Package Builder ==="
echo ""

# ── 1. Build Rust backend (release) ──
echo "[1/7] Building Rust backend (release)..."
cd "$ROOT"
cargo build -p starriver-desktop --release
DYLIB="$ROOT/target/release/libstarriver_desktop.dylib"
NODE_FILE="$ROOT/target/release/starriver_desktop.node"
if [ ! -f "$DYLIB" ]; then
    echo "ERROR: Build failed, dylib not found"
    exit 1
fi
echo "  Rust build OK"

# ── 2. Copy dylib → .node ──
echo ""
echo "[2/7] Preparing .node file..."
cp "$DYLIB" "$NODE_FILE"
echo "  $NODE_FILE ($(ls -lh "$NODE_FILE" | awk '{print $5}'))"

# ── 3. Ensure app icon exists (缺失时兜底) ──
# 正式图标由 desktop/scripts/make-logo.py 生成（固定种子可复现，见
# app-logo-pipeline.md）；此处仅在 icon.png 缺失时兜底，绝不覆盖已有
# 图标（旧守卫查 icon.icns 而 mac 配置用的是 icon.png，导致每次打包
# 都把正式图标覆盖成无效的占位 PNG，app-builder 解码崩溃）。
echo ""
echo "[3/7] Checking app icon..."
ICON_DIR="$ELECTRON_DIR/assets"
mkdir -p "$ICON_DIR"
if [ ! -f "$ICON_DIR/icon.png" ]; then
    echo "  No icon.png found, generating via make-logo.py..."
    (cd "$ROOT/desktop" && python3 scripts/make-logo.py)
    if [ ! -f "$ICON_DIR/icon.png" ]; then
        echo "ERROR: icon.png missing and make-logo.py failed (PIL required)"
        exit 1
    fi
else
    echo "  icon.png present ($(ls -lh "$ICON_DIR/icon.png" | awk '{print $5}'))"
fi

# ── 4. Install npm dependencies ──
echo ""
echo "[4/7] Installing npm dependencies..."
cd "$ELECTRON_DIR"
if [ -d "node_modules/electron" ]; then
    npm install --prefer-offline 2>&1 | tail -3
else
    npm install 2>&1 | tail -5
fi
echo "  npm install done"

# ── 5. Compile TypeScript ──
echo ""
echo "[5/7] Compiling TypeScript..."
npx tsc 2>&1 && npx esbuild renderer/app.tsx --bundle --outfile=renderer/bundle.js --platform=browser --format=iife 2>&1
# preload 必须重打：sandboxed preload 不能相对 require，tsc 产物要被自包含
# esbuild bundle 覆盖（见 renderer/__legacy_bridge.test.ts）。漏掉这步会把
# 上一次的旧 preload.js 打进包（2026-11-15：runtime:event 扇出修复没进包）。
npx esbuild preload.ts --bundle --platform=node --format=cjs --external:electron --outfile=preload.js 2>&1
echo "  TypeScript compilation OK"

# ── 6. Build daemon (starriverd) + ACP server ──
# electron-builder 的 extraResources 直接从 target/release/ 收集 starriverd /
# starriver-acp。本脚本此前只重建 NAPI，daemon 往往是上次构建的旧二进制
# （2026-11-14 事故：DMG 带旧 starriverd，协议无 iconSvg 等新字段）。与
# 三平台 package.sh 的 build_daemon/build_acp mac 分支保持一致：daemon 构建失败
# 硬失败，ACP 失败仅警告（桌面端自身不依赖 ACP）。
echo ""
echo "[6/7] Building daemon (starriverd) + ACP server..."
cd "$ROOT"
if cargo build -p starriverd --release; then
    strip -x "$RELEASE/starriverd"
    echo "  starriverd OK ($(ls -lh "$RELEASE/starriverd" | awk '{print $5}'))"
else
    echo "ERROR: starriverd build failed"
    exit 1
fi
if cargo build -p starriver-acp --release 2>/dev/null; then
    strip -x "$RELEASE/starriver-acp" 2>/dev/null || true
    echo "  starriver-acp OK ($(ls -lh "$RELEASE/starriver-acp" | awk '{print $5}'))"
else
    echo "  WARNING: starriver-acp build failed — 包不带 ACP 服务器"
fi

# ── 7. Package with electron-builder ──
echo ""
echo "[7/7] Building DMG package..."
cd "$ELECTRON_DIR"
rm -rf "$DIST_DIR"
npx electron-builder --mac --arm64 2>&1 | grep -E "building|packed|blockmap|Done" || npx electron-builder --mac --arm64

# ── Result ──
echo ""
echo "=== Package Complete ==="
if [ -f "$DIST_DIR"/*.dmg ]; then
    DMG=$(ls -t "$DIST_DIR"/*.dmg 2>/dev/null | head -1)
    echo "DMG:  $DMG ($(ls -lh "$DMG" | awk '{print $5}'))"
    echo ""
    echo "Install: open $DMG"
fi
echo ""
echo "Output directory: $DIST_DIR"
ls -lh "$DIST_DIR"/ 2>/dev/null
