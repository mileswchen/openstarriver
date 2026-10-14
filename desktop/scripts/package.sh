#!/bin/bash
# Starriver Desktop — 三平台安装包打包脚本
#
#   bash package.sh mac          → macOS DMG + zip（arm64，原生构建；Developer ID 签名 + 公证 + 装订）
#   bash package.sh linux        → Linux AppImage + deb（x86_64；Linux 主机原生 cargo，其余主机 zigbuild 交叉）
#   bash package.sh win          → Windows NSIS setup.exe（x86_64 交叉，zigbuild + wine）
#   bash package.sh all          → 以上三个按序执行
#
# 输出目录：desktop/electron/dist/
# 依赖：cargo-zigbuild（brew install cargo-zigbuild）、zig（brew install zig）、
#       wine（仅 win；brew install --cask wine-stable）。
#
# 网络：electron-builder 默认从 github 拉 Electron 分发包与打包工具；在受限网络可设
#       ELECTRON_MIRROR 与 ELECTRON_BUILDER_BINARIES_MIRROR 加速，例如：
#         ELECTRON_MIRROR=https://npmmirror.com/mirrors/electron/ \
#         ELECTRON_BUILDER_BINARIES_MIRROR=https://npmmirror.com/mirrors/electron-builder-binaries/ \
#         bash package.sh all
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ELECTRON_DIR="$ROOT/desktop/electron"
DIST_DIR="$ELECTRON_DIR/dist"
RELEASE="$ROOT/target/release"
NODE_FILE="$RELEASE/starriver_desktop.node"

# WI-152: workspace-write 沙箱会把系统临时目录禁掉(electron-builder 的
# mkdtemp 第一步就死)。探测一次;不可写就落到仓库内 .tmp-build/tmp。
if ! mktemp -d "${TMPDIR:-/tmp}/starriver-tmp-probe-XXXXXX" >/dev/null 2>&1; then
  mkdir -p "$ROOT/.tmp-build/tmp"
  TMPDIR="$ROOT/.tmp-build/tmp"
  export TMPDIR TMP="$TMPDIR" TEMP="$TMPDIR"
  echo "[env] system tmp not writable — TMPDIR -> $TMPDIR"
fi

TARGET="${1:-mac}"
case "$TARGET" in
  mac)   PLATFORM_STR="macOS (arm64, native)" ;;
  linux) PLATFORM_STR="Linux (x86_64, cross)" ;;
  win)   PLATFORM_STR="Windows (x86_64, cross)" ;;
  all)   PLATFORM_STR="all three platforms" ;;
  *) echo "Usage: bash package.sh [mac|linux|win|all]"; exit 1 ;;
esac

echo "=== Starriver Desktop — package: $PLATFORM_STR ==="

build_napi() {
  # $1 = platform; compiles the NAPI cdylib and prints the artifact path on
  # STDOUT only — progress messages go to stderr so `$(build_napi ...)` captures
  # just the path.
  case "$1" in
    mac)
      echo "[napi] cargo build -p starriver-desktop --release" >&2
      (cd "$ROOT" && cargo build -p starriver-desktop --release)
      echo "$RELEASE/libstarriver_desktop.dylib"
      ;;
    linux)
      if [ "$(uname -s)" = "Linux" ] && [ "$(uname -m)" = "x86_64" ]; then
        # Native Linux build host: plain cargo already targets
        # x86_64-unknown-linux-gnu — zigbuild adds nothing here.
        echo "[napi] native Linux x86_64 — cargo build --release" >&2
        (cd "$ROOT" && cargo build --release -p starriver-desktop)
        echo "$RELEASE/libstarriver_desktop.so"
      else
        echo "[napi] cargo zigbuild --target x86_64-unknown-linux-gnu --release" >&2
        (cd "$ROOT" && cargo zigbuild --target x86_64-unknown-linux-gnu --release -p starriver-desktop)
        echo "$ROOT/target/x86_64-unknown-linux-gnu/release/libstarriver_desktop.so"
      fi
      ;;
    win)
      setup_libnode || { echo "ERROR: libnode import lib setup failed" >&2; exit 1; }
      echo "[napi] cargo zigbuild --target x86_64-pc-windows-gnu --release" >&2
      (cd "$ROOT" && cargo zigbuild --target x86_64-pc-windows-gnu --release -p starriver-desktop)
      echo "$ROOT/target/x86_64-pc-windows-gnu/release/starriver_desktop.dll"
      ;;
  esac
}

sync_node() {
  # $1 = platform, $2 = artifact path. Copy to the canonical <target>/release/*.node
  # that electron-builder `files` references. macOS needs ad-hoc codesign or the
  # dylib gets SIGKILLed on load.
  local platform="$1"
  local artifact="$2"
  if [ ! -f "$artifact" ]; then
    echo "ERROR: NAPI artifact not found: $artifact" >&2
    exit 1
  fi
  cp "$artifact" "$NODE_FILE"
  echo "[napi] synced -> $NODE_FILE ($(ls -lh "$NODE_FILE" | awk '{print $5}'))"
  if [[ "$platform" == "mac" ]]; then
    # workspace release 关闭了 cargo strip（rust#157750：llvm-objcopy 产出错位的
    # LINKEDIT 字符串池，macOS 27 dyld 拒载），这里用系统 strip 压符号——它保持对齐。
    strip -x "$NODE_FILE"
    codesign --force --sign - "$NODE_FILE" && echo "[napi] stripped + ad-hoc codesigned (macOS)"
  fi
}

build_daemon() {
  # $1 = platform. 编译 starriverd（FPM P4b daemon）并同步到 target/release/
  # 供 electron-builder extraResources 收集（与 NAPI 同路径约定）。
  # Windows 交叉构建失败时只警告：Windows 包可退回 embedded（daemon 优先 macOS/Linux）。
  local p="$1"
  local out=""
  case "$p" in
    mac)
      echo "[daemon] cargo build -p starriverd --release" >&2
      (cd "$ROOT" && cargo build -p starriverd --release)
      out="$RELEASE/starriverd"
      ;;
    linux)
      if [ "$(uname -s)" = "Linux" ] && [ "$(uname -m)" = "x86_64" ]; then
        # Native Linux host: cargo build emits at $RELEASE directly — the cp
        # the cross branch needs would be source==destination.
        echo "[daemon] native Linux x86_64 — cargo build --release" >&2
        (cd "$ROOT" && cargo build --release -p starriverd)
      else
        echo "[daemon] cargo zigbuild --target x86_64-unknown-linux-gnu --release -p starriverd" >&2
        (cd "$ROOT" && cargo zigbuild --target x86_64-unknown-linux-gnu --release -p starriverd)
        cp "$ROOT/target/x86_64-unknown-linux-gnu/release/starriverd" "$RELEASE/starriverd"
      fi
      out="$RELEASE/starriverd"
      ;;
    win)
      echo "[daemon] cargo zigbuild --target x86_64-pc-windows-gnu --release -p starriverd" >&2
      if (cd "$ROOT" && cargo zigbuild --target x86_64-pc-windows-gnu --release -p starriverd); then
        cp "$ROOT/target/x86_64-pc-windows-gnu/release/starriverd.exe" "$RELEASE/starriverd.exe"
        out="$RELEASE/starriverd.exe"
      else
        echo "[daemon] WARNING: starriverd.exe build failed — Windows 包不带 daemon（embedded 兜底）" >&2
        return 0
      fi
      ;;
  esac
  if [ -f "$out" ]; then
    # 同 sync_node：cargo strip 已关（rust#157750），macOS 用系统 strip 压符号（保持对齐）
    if [[ "$p" == "mac" ]]; then strip -x "$out"; fi
    echo "[daemon] -> $out ($(ls -lh "$out" | awk '{print $5}'))"
  else
    echo "ERROR: daemon artifact missing: $out" >&2
    return 1
  fi
}

build_acp() {
  # $1 = platform. ACP 服务器（WI-70，编辑器/自动化集成入口）：与 starriverd
  # 同链路编译并随安装包进 resources/。构建失败只警告——ACP 是增量通道，
  # 桌面端自身不依赖它。
  local p="$1"
  case "$p" in
    mac)
      # Native build: cargo already emitted the binary at $RELEASE — the cp
      # the cross branches need would be source==destination ("identical, not
      # copied"), which broke the && chain and misreported a build failure
      # (2026-10-03). Strip in place; the binary ships via extraResources.
      echo "[acp] cargo build -p starriver-acp --release" >&2
      (cd "$ROOT" && cargo build -p starriver-acp --release) \
        && strip -x "$RELEASE/starriver-acp" \
        && echo "[acp] -> $RELEASE/starriver-acp ($(ls -lh "$RELEASE/starriver-acp" | awk '{print $5}'))" >&2 \
        || echo "[acp] WARNING: starriver-acp build failed — 包不带 ACP 服务器" >&2
      ;;
    linux)
      if [ "$(uname -s)" = "Linux" ] && [ "$(uname -m)" = "x86_64" ]; then
        # Native Linux host: cargo build emits at $RELEASE directly — the cp
        # the cross branch needs would be source==destination (mirrors the
        # mac-native fix above).
        echo "[acp] native Linux x86_64 — cargo build --release" >&2
        (cd "$ROOT" && cargo build --release -p starriver-acp) \
          && echo "[acp] -> $RELEASE/starriver-acp ($(ls -lh "$RELEASE/starriver-acp" | awk '{print $5}'))" >&2 \
          || echo "[acp] WARNING: starriver-acp build failed — 包不带 ACP 服务器" >&2
      else
        echo "[acp] cargo zigbuild --target x86_64-unknown-linux-gnu --release -p starriver-acp" >&2
        (cd "$ROOT" && cargo zigbuild --target x86_64-unknown-linux-gnu --release -p starriver-acp) \
          && cp "$ROOT/target/x86_64-unknown-linux-gnu/release/starriver-acp" "$RELEASE/starriver-acp" \
          && echo "[acp] -> $RELEASE/starriver-acp ($(ls -lh "$RELEASE/starriver-acp" | awk '{print $5}'))" >&2 \
          || echo "[acp] WARNING: starriver-acp build failed — 包不带 ACP 服务器" >&2
      fi
      ;;
    win)
      echo "[acp] cargo zigbuild --target x86_64-pc-windows-gnu --release -p starriver-acp" >&2
      (cd "$ROOT" && cargo zigbuild --target x86_64-pc-windows-gnu --release -p starriver-acp) \
        && cp "$ROOT/target/x86_64-pc-windows-gnu/release/starriver-acp.exe" "$RELEASE/starriver-acp.exe" \
        && echo "[acp] -> $RELEASE/starriver-acp.exe ($(ls -lh "$RELEASE/starriver-acp.exe" | awk '{print $5}'))" >&2 \
        || echo "[acp] WARNING: starriver-acp.exe build failed — 包不带 ACP 服务器" >&2
      ;;
  esac
}

build_sbx_helper() {
  # $1 = platform. Windows 沙箱 helper（writetoken 后端）：随安装包进 resources/，
  # 运行期 helper_path 以 <install>/resources 候选发现。仅 win 产出真身；
  # 构建失败只警告——Windows 沙箱将降级为审批门控（docs/design/33 §十一）。
  local p="$1"
  [ "$p" = "win" ] || return 0
  echo "[sbx] cargo zigbuild --target x86_64-pc-windows-gnu --release -p starriver-sandbox --bin starriver-sbx-helper" >&2
  if (cd "$ROOT" && cargo zigbuild --target x86_64-pc-windows-gnu --release -p starriver-sandbox --bin starriver-sbx-helper); then
    cp "$ROOT/target/x86_64-pc-windows-gnu/release/starriver-sbx-helper.exe" "$RELEASE/starriver-sbx-helper.exe"
    echo "[sbx] -> $RELEASE/starriver-sbx-helper.exe ($(ls -lh "$RELEASE/starriver-sbx-helper.exe" | awk '{print $5}'))" >&2
  else
    echo "[sbx] WARNING: starriver-sbx-helper.exe build failed — Windows 包沙箱降级为审批" >&2
  fi
}

setup_binutils_path() {
  # fpm（deb）在 macOS 26+ 上用系统 /usr/bin/ar 会产出空包（只有 __.SYMDEF SORTED，
  # 成员全部丢失）。GNU binutils 的 `gar` 能正确产出 deb 需要的 ar 容器。binutils 是
  # keg-only，未进默认 PATH，这里把它补上供 fpm 探测（fpm 依次尝试 ar/gar）。
  local binutils_bin="$(brew --prefix 2>/dev/null)/opt/binutils/bin"
  if [ -x "$binutils_bin/gar" ]; then
    export PATH="$binutils_bin:$PATH"
    echo "[linux] GNU ar on PATH: $binutils_bin/gar ($(command -v gar))"
  else
    echo "[linux] WARNING: binutils not found (brew install binutils) — deb 可能产出空包" >&2
  fi
}

setup_libnode() {
  # Windows NAPI 交叉编译需要 libnode 导入库：
  #   - napi-build 的 windows.rs 探测 `libnode.dll` 文件是否存在（否则 panic）
  #   - zig ld 用 `-lnode` 时按 paths_first 找 `node.lib` / `node.dll` / `libnode.a`
  # 这里从 npmmirror 拉 Node 头文件，解析 NAPI 导出符号，用 zig dlltool 生成 GNU 导入库。
  # 产物缓存在 <repo>/target/win-libnode/，幂等。只设置 LIBNODE_PATH，不构建。
  local dir="$ROOT/target/win-libnode"
  local node_ver="v18.18.2"   # Electron 28 内嵌的 Node 版本；NAPI ABI 跨版本稳定
  if [ -f "$dir/libnode.a" ]; then
    export LIBNODE_PATH="$dir"
    echo "[napi] libnode import lib cached at $dir" >&2
    return 0
  fi
  echo "[napi] generating libnode import lib for windows-gnu (node $node_ver)…" >&2
  mkdir -p "$dir"
  local hdr_tmp="$(mktemp -d)"
  curl -fsSL --max-time 120 \
    "https://npmmirror.com/mirrors/node/$node_ver/node-$node_ver-headers.tar.gz" \
    -o "$hdr_tmp/hdrs.tgz" || { echo "ERROR: fetch node headers failed" >&2; rm -rf "$hdr_tmp"; return 1; }
  tar xzf "$hdr_tmp/hdrs.tgz" -C "$hdr_tmp"
  local inc="$hdr_tmp/node-$node_ver/include/node"
  # 解析 NAPI 导出函数名（NAPI_EXTERN 声明）
  {
    grep -oE "NAPI_EXTERN[^;]*napi_[a-zA-Z0-9_]+" "$inc/js_native_api.h" 2>/dev/null
  } | grep -oE "napi_[a-zA-Z0-9_]+" | sort -u > "$dir/exports.txt"
  # 注册函数：node_api_module_register / napi_module_register 声明形式不同，显式补上
  { echo "napi_module_register"; echo "node_api_module_register"; } >> "$dir/exports.txt"
  sort -u "$dir/exports.txt" -o "$dir/exports.txt"
  # .def → GNU 导入库
  {
    echo "LIBRARY node.exe"
    echo "EXPORTS"
    sed 's/^/    /' "$dir/exports.txt"
  } > "$dir/node.def"
  ( cd "$dir" && zig dlltool -d node.def -l libnode.dll.a -m i386:x86-64 -D node.exe ) || { echo "ERROR: zig dlltool failed" >&2; rm -rf "$hdr_tmp"; return 1; }
  cp "$dir/libnode.dll.a" "$dir/libnode.a"
  cp "$dir/libnode.dll.a" "$dir/node.lib"
  ln -sf libnode.dll.a "$dir/libnode.dll"
  rm -rf "$hdr_tmp"
  export LIBNODE_PATH="$dir"
  echo "[napi] libnode import lib ready at $dir ($(stat -f %z "$dir/libnode.a" 2>/dev/null || stat -c %s "$dir/libnode.a") bytes, $(wc -l < "$dir/exports.txt") exports)" >&2
}

setup_wine_path() {
  # wine 装完后可能不在 PATH，把常见位置补进去供 electron-builder 的 makensis 使用。
  # 新 wine 合并了 wine64 → 只有一个 `wine` 启动器，两者都探测。
  local candidates=(
    "/Applications/Wine Stable.app/Contents/Resources/wine/bin"
    "$(brew --prefix 2>/dev/null)/bin"
  )
  for c in "${candidates[@]}"; do
    if [ -d "$c" ] && { command -v "$c/wine64" >/dev/null 2>&1 || command -v "$c/wine" >/dev/null 2>&1; }; then
      export PATH="$c:$PATH"
      echo "[win] wine found at $c ($(command -v "$c/wine64" 2>/dev/null || echo "$c/wine"))"
      return 0
    fi
  done
  echo "[win] WARNING: wine not on PATH — NSIS 构建可能失败" >&2
}

build_frontend() {
  echo "[frontend] bundle + tsc + bundle:preload"
  cd "$ELECTRON_DIR"
  npm run bundle
  npx tsc
  # sandboxed preload 不能相对 require，必须打成单文件；须在 tsc 之后覆盖其输出。
  npm run bundle:preload
}

notarize_mac() {
  # Developer ID 签名后的公证 + 装订（正式分发必需；跳过公证的包在其它 Mac
  # 上首次打开会被 Gatekeeper 拒）。凭据来自 notarytool store-credentials
  # 存入钥匙串的 profile（默认 starriver-notary）；NOTARIZE=0 跳过。
  # 流程与 2026-10-02 实测一致：app 打 zip（ditto）→ notarytool submit --wait
  # → stapler staple 回写 .app → spctl 验证 → 用已装订 app 重打 DMG
  # （electron-builder 先产的 DMG 内是不带装订票的 app，必须覆盖）。
  if [ "${NOTARIZE:-1}" != "1" ]; then
    echo "[notary] NOTARIZE=0 — 跳过公证"
    return 0
  fi
  if ! command -v xcrun >/dev/null 2>&1; then
    echo "[notary] WARNING: xcrun 不可用 — 跳过公证" >&2
    return 0
  fi
  local profile="${NOTARY_PROFILE:-starriver-notary}"
  local app="$DIST_DIR/mac-arm64/Starriver.app"
  local zip_path="$DIST_DIR/Starriver-mac-notarize.zip"
  [ -d "$app" ] || { echo "[notary] WARNING: $app 不存在 — 跳过公证" >&2; return 0; }
  ditto -c -k --keepParent --sequesterRsrc --zlibCompressionLevel 9 "$app" "$zip_path"
  echo "[notary] notarytool submit (profile: $profile, --wait)"
  if ! xcrun notarytool submit "$zip_path" --keychain-profile "$profile" --wait; then
    echo "[notary] ERROR: 公证失败 — 检查会员协议/凭据（NOTARIZE=0 可跳过公证）" >&2
    rm -f "$zip_path"
    return 1
  fi
  if ! xcrun stapler staple "$app"; then
    echo "[notary] ERROR: stapler 装订失败" >&2
    return 1
  fi
  if spctl -a -vv "$app" 2>&1 | grep -q accepted; then
    echo "[notary] Gatekeeper: accepted"
  else
    echo "[notary] WARNING: spctl 未 accepted — 请人工核查" >&2
  fi
  rm -f "$zip_path"
  local version
  version=$(node -p "require('$ELECTRON_DIR/package.json').version")
  local dmg="$DIST_DIR/Starriver-$version-arm64.dmg"
  echo "[notary] 用已装订 app 重打 DMG -> $dmg"
  rm -f "$dmg"
  # 从暂存目录重打,而不是直接 -srcfolder <app>:裸 app 会丢掉
  # Applications 快捷方式,装出来的 DMG 只有一个孤零零的图标,无法拖拽
  # 安装(2026-10-03 用户报告)。暂存 = 装订后的 app + /Applications 符号
  # 链接;cp -pR 保留签名与装订票,hdiutil 保留符号链接本体。
  local stage
  stage=$(mktemp -d)
  cp -pR "$app" "$stage/Starriver.app"
  ln -s /Applications "$stage/Applications"
  # 拖拽安装布局:预生成的 .DS_Store 模板(图标视图、128px、Starriver 左 /
  # Applications 右)。模板用 Finder 脚本一次性生成(见 assets/dmg/),构建时
  # 只拷贝——不依赖运行时 AppleScript/TCC 权限。
  local ds_template="$ELECTRON_DIR/assets/dmg/ds-store-template"
  if [ -f "$ds_template" ]; then
    cp "$ds_template" "$stage/.DS_Store"
  else
    echo "[notary] WARNING: 缺 $ds_template — DMG 用 Finder 默认布局" >&2
  fi
  if hdiutil create -srcfolder "$stage" -volname "Starriver" -format UDZO -o "$dmg" >/dev/null; then
    echo "[notary] done: $dmg (app + Applications)"
  else
    echo "[notary] ERROR: DMG 重打失败" >&2
    rm -rf "$stage"
    return 1
  fi
  rm -rf "$stage"
  # 自检:挂载验证拖拽安装布局(Applications 链接必须在)。2026-10-03 事故:
  # 03:11 的 DMG 又变裸包(旧命令/旧脚本缓冲重跑),用户装不了才发现。
  # 从此产物缺链接直接判失败,不允许带病出包。
  local mnt
  mnt=$(mktemp -d)/v
  if hdiutil attach -nobrowse -readonly -mountpoint "$mnt" "$dmg" >/dev/null 2>&1; then
    if [ ! -L "$mnt/Applications" ]; then
      echo "[notary] ERROR: 产物自检失败 — DMG 缺 Applications 快捷方式,不能拖拽安装" >&2
      hdiutil detach "$mnt" >/dev/null 2>&1
      return 1
    fi
    hdiutil detach "$mnt" >/dev/null 2>&1
    echo "[notary] 产物自检: Applications 链接在位"
  else
    echo "[notary] WARNING: 产物自检挂载失败,跳过布局检查" >&2
  fi
}

run_electron_builder() {
  local target="$1"
  local extra=()
  case "$target" in
    mac)   extra=(--mac --arm64) ;;
    linux) extra=(--linux --x64); setup_binutils_path ;;  # 强制 x64 + GNU ar（修 deb 空包）
    win)   extra=(--win --x64); setup_wine_path ;;
  esac
  echo "[builder] npx electron-builder ${extra[*]}"
  cd "$ELECTRON_DIR"
  # ELECTRON_BUILDER_EXTRA: 追加 CLI 参数（远程构建农场传 --config.mac.identity=null
  # 跳过 Developer ID 签名——证书在开发机钥匙串里，mini 只出 adhoc 包）。
  # 不加引号以支持多参数按空白拆分。
  # Retry ×3: codesign --timestamp hits Apple's timestamp server per file; a
  # transient network blip mid-bundle ("The timestamp service is not
  # available", 2026-10-03) killed an otherwise fine build at bn.lproj.
  local attempt=1
  until npx electron-builder "${extra[@]}" ${ELECTRON_BUILDER_EXTRA:-}; do
    if [ "$attempt" -ge 3 ]; then
      echo "[builder] ERROR: electron-builder failed after $attempt attempts" >&2
      return 1
    fi
    echo "[builder] attempt $attempt failed — retrying in 15s" >&2
    sleep 15
    attempt=$((attempt + 1))
  done
}

package_one() {
  # $1 = platform
  local p="$1"
  echo ""
  echo "────────────────────────── $p ──────────────────────────"
  build_daemon "$p"
  build_acp "$p"
  build_sbx_helper "$p"
  sync_node "$p" "$(build_napi "$p")"
  python3 "$ROOT/desktop/scripts/gen-icons.py"
  build_frontend
  run_electron_builder "$p"
  if [ "$p" = "mac" ]; then
    notarize_mac
    cleanup_unpacked
  fi
  echo ""
  echo "=== $p package done ==="
}

# 2026-10-04: 出包成功后删掉 mac 解包目录。dist/mac-arm64/Starriver.app 会被
# Spotlight 当正经应用索引:开发机每出一次包,"应用"窗口/聚焦搜索就多一个可点的
# Starriver 图标,与已安装副本混淆(本次排查的幽灵图标来源),误点还会绕过单实例
# 锁造成双实例。安装包产物(DMG/zip)不受影响;调试需要解包副本时 KEEP_UNPACKED=1。
cleanup_unpacked() {
  local dir="$DIST_DIR/mac-arm64"
  if [ "${KEEP_UNPACKED:-0}" = "1" ]; then
    echo "[cleanup] KEEP_UNPACKED=1 — 保留 $dir"
    return 0
  fi
  if [ -d "$dir" ]; then
    rm -rf "$dir"
    echo "[cleanup] 已删除解包目录 $dir(防 Spotlight/启动台幽灵图标)"
  fi
}

if [ "$TARGET" = "all" ]; then
  for p in mac linux win; do
    package_one "$p"
  done
else
  package_one "$TARGET"
fi

echo ""
echo "=== 产物清单 ($DIST_DIR) ==="
ls -lh "$DIST_DIR"/ 2>/dev/null | grep -vE "^total|^d" || true
