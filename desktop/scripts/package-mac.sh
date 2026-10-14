#!/bin/bash
# Starriver Desktop — Mac 安装包打包脚本
# 输出: desktop/electron/dist/Starriver-*.dmg
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ELECTRON_DIR="$ROOT/desktop/electron"
DIST_DIR="$ELECTRON_DIR/dist"

echo "=== Starriver Desktop — Mac DMG Package Builder ==="
echo ""

# ── 1. Build Rust backend (release) ──
echo "[1/6] Building Rust backend (release)..."
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
echo "[2/6] Preparing .node file..."
cp "$DYLIB" "$NODE_FILE"
echo "  $NODE_FILE ($(ls -lh "$NODE_FILE" | awk '{print $5}'))"

# ── 3. Ensure app icon exists (缺失时兜底) ──
# 正式图标由 desktop/scripts/make-logo.py 生成（固定种子可复现，见
# app-logo-pipeline.md）；此处仅在 icon.png 缺失时兜底，绝不覆盖已有
# 图标（旧守卫查 icon.icns 而 mac 配置用的是 icon.png，导致每次打包
# 都把正式图标覆盖成无效的占位 PNG，app-builder 解码崩溃）。
echo ""
echo "[3/6] Checking app icon..."
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
echo "[4/6] Installing npm dependencies..."
cd "$ELECTRON_DIR"
if [ -d "node_modules/electron" ]; then
    npm install --prefer-offline 2>&1 | tail -3
else
    npm install 2>&1 | tail -5
fi
echo "  npm install done"

# ── 5. Compile TypeScript ──
echo ""
echo "[5/6] Compiling TypeScript..."
npx tsc 2>&1 && npx esbuild renderer/app.tsx --bundle --outfile=renderer/bundle.js --platform=browser --format=iife 2>&1
echo "  TypeScript compilation OK"

# ── 6. Package with electron-builder ──
echo ""
echo "[6/6] Building DMG package..."
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
