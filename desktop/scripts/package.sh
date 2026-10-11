#!/bin/bash
# Starriver Desktop — three-platform installer packaging script
#
#   bash package.sh mac          → macOS DMG + zip (arm64, native build; Developer ID signing + notarization + stapling)
#   bash package.sh linux        → Linux AppImage + deb (x86_64; native cargo on Linux hosts, zigbuild cross-compile elsewhere)
#   bash package.sh win          → Windows NSIS setup.exe (x86_64 cross-compile, zigbuild + wine)
#   bash package.sh all          → all of the above, in order
#
# Output directory: desktop/electron/dist/
# Dependencies: cargo-zigbuild (brew install cargo-zigbuild), zig (brew install zig),
#       wine (win only; brew install --cask wine-stable).
#
# Network: electron-builder pulls the Electron distribution and packaging tools from github
#       by default; on restricted networks set ELECTRON_MIRROR and ELECTRON_BUILDER_BINARIES_MIRROR to speed it up, e.g.:
#         ELECTRON_MIRROR=https://npmmirror.com/mirrors/electron/ \
#         ELECTRON_BUILDER_BINARIES_MIRROR=https://npmmirror.com/mirrors/electron-builder-binaries/ \
#         bash package.sh all
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ELECTRON_DIR="$ROOT/desktop/electron"
DIST_DIR="$ELECTRON_DIR/dist"
RELEASE="$ROOT/target/release"
NODE_FILE="$RELEASE/starriver_desktop.node"

# WI-152: workspace-write sandboxes lock down the system temp dir (electron-builder's
# mkdtemp dies on the first call). Probe once; on failure fall back to <repo>/.tmp-build/tmp.
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

# /usr/bin/strip is an xcode-select shim: it probes `xcodebuild -find strip
# 2> /dev/null`, and workspace-write sandboxes deny the /dev/null redirect —
# the shim dies (exit 72) and even triggers a bogus CLT-install prompt. The
# toolchain's real strip needs no probe. Prefer it; fall back to the shim;
# never hard-fail the package over symbol compression (rust#157750 workaround).
strip_mac() {
  local f="$1"
  local ts="/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/strip"
  if [ -x "$ts" ] && "$ts" -x "$f"; then return 0; fi
  if strip -x "$f"; then return 0; fi
  echo "[strip] WARNING: strip failed for $f — shipping unstripped" >&2
  return 0
}

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
    # workspace release disables cargo strip (rust#157750: llvm-objcopy emits a mis-aligned
    # LINKEDIT string pool that macOS 27 dyld rejects); compress symbols here with the system strip — it keeps alignment.
    strip_mac "$NODE_FILE" && echo "[napi] stripped + $(codesign --force --sign - "$NODE_FILE" && echo 'ad-hoc codesigned (macOS)')"
  fi
}

build_daemon() {
  # $1 = platform. Build starriverd (FPM P4b daemon) and sync it into target/release/
  # for electron-builder extraResources to collect (same path convention as NAPI).
  # A failed Windows cross-build only warns: the Windows package falls back to embedded (daemon preferred on macOS/Linux).
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
  # Same as sync_node: cargo strip disabled (rust#157750); macOS uses system strip to compress symbols (keeps alignment).
    if [[ "$p" == "mac" ]]; then strip_mac "$out"; fi
    echo "[daemon] -> $out ($(ls -lh "$out" | awk '{print $5}'))"
  else
    echo "ERROR: daemon artifact missing: $out" >&2
    return 1
  fi
}

build_acp() {
  # $1 = platform. ACP server (WI-70, editor/automation integration entry): built on the same
  # chain as starriverd and shipped in resources/. Failure only warns — ACP is an incremental
  # channel; the desktop app itself does not depend on it.
  local p="$1"
  case "$p" in
    mac)
      # Native build: cargo already emitted the binary at $RELEASE — the cp
      # the cross branches need would be source==destination ("identical, not
      # copied"), which broke the && chain and misreported a build failure
      # (2026-10-03). Strip in place; the binary ships via extraResources.
      echo "[acp] cargo build -p starriver-acp --release" >&2
      (cd "$ROOT" && cargo build -p starriver-acp --release) \
        && strip_mac "$RELEASE/starriver-acp" \
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
  # $1 = platform. Windows sandbox helper (writetoken backend): shipped in resources/,
  # discovered at runtime via <install>/resources candidates in helper_path. Only win produces
  # the real binary; failure only warns — Windows sandbox degrades to approval gating (docs/design/33 §11).
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
  # fpm (deb) on macOS 26+ produces an empty archive with the system /usr/bin/ar (only
  # __.SYMDEF SORTED, all members lost). GNU binutils' `gar` produces the ar container deb
  # needs. binutils is keg-only and not on the default PATH — prepend it here for fpm to probe (fpm tries ar/gar in order).
  local binutils_bin="$(brew --prefix 2>/dev/null)/opt/binutils/bin"
  if [ -x "$binutils_bin/gar" ]; then
    export PATH="$binutils_bin:$PATH"
    echo "[linux] GNU ar on PATH: $binutils_bin/gar ($(command -v gar))"
  else
    echo "[linux] WARNING: binutils not found (brew install binutils) — deb 可能产出空包" >&2
  fi
}

setup_libnode() {
  # Windows NAPI cross-compilation needs the libnode import library:
  #   - napi-build's windows.rs probes for a `libnode.dll` file (panics otherwise)
  #   - zig ld resolves `node.lib` / `node.dll` / `libnode.a` for `-lnode` (paths_first)
  # Node headers are pulled from npmmirror here, NAPI export symbols parsed, and a GNU import
  # library generated with zig dlltool. Cached at <repo>/target/win-libnode/, idempotent. Only sets LIBNODE_PATH, does not build.
  local dir="$ROOT/target/win-libnode"
  local node_ver="v18.18.2"   # Node version embedded in Electron 28; NAPI ABI stable across versions
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
  # Parse NAPI export function names (NAPI_EXTERN declarations)
  {
    grep -oE "NAPI_EXTERN[^;]*napi_[a-zA-Z0-9_]+" "$inc/js_native_api.h" 2>/dev/null
  } | grep -oE "napi_[a-zA-Z0-9_]+" | sort -u > "$dir/exports.txt"
  # Registration functions: node_api_module_register / napi_module_register differ in declaration form — add both explicitly
  { echo "napi_module_register"; echo "node_api_module_register"; } >> "$dir/exports.txt"
  sort -u "$dir/exports.txt" -o "$dir/exports.txt"
  # .def → GNU import library
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
  # wine may not be on PATH right after install — add the common locations for electron-builder's makensis.
  # New wine merged wine64 → a single `wine` launcher; probe both.
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
  # sandboxed preload cannot require relatively — must be bundled single-file; overwrite tsc output after tsc runs.
  npm run bundle:preload
}

notarize_mac() {
  # Notarization + stapling after Developer ID signing (required for official distribution;
  # packages without notarization get Gatekeeper-rejected on other Macs on first open). Credentials come from
  # a notarytool store-credentials keychain profile (default starriver-notary); NOTARIZE=0 skips.
  # Flow verified 2026-10-02: zip the app (ditto) → notarytool submit --wait
  # → stapler staple back into .app → spctl verify → rebuild the DMG from the stapled app
  # (the DMG electron-builder produces first contains an app without the staple ticket and must be overwritten).
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
  # Rebuild from a staging dir instead of -srcfolder <app> directly: a bare app loses the
  # Applications symlink and the DMG installs as a lonely icon that cannot be dragged
  # (user report 2026-10-03). Staging = stapled app + /Applications symlink;
  # cp -pR keeps the signature and staple ticket, hdiutil keeps the symlink itself.
  local stage
  stage=$(mktemp -d)
  cp -pR "$app" "$stage/Starriver.app"
  ln -s /Applications "$stage/Applications"
  # Drag-install layout: a pre-generated .DS_Store template (icon view, 128px, Starriver left /
  # Applications right). The template is generated once via Finder scripting (see assets/dmg/);
  # at build time it is only copied — no runtime AppleScript/TCC permissions needed.
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
  # Self-check: mount and verify the drag-install layout (the Applications symlink must exist). 2026-10-03 incident:
  # the 03:11 DMG was a bare package again (old command/stale script rerun); found only when the user could not install.
  # Since then a missing symlink in the artifact is a hard failure — no shipping broken packages.
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
    linux) extra=(--linux --x64); setup_binutils_path ;;  # force x64 + GNU ar (fixes empty deb)
    win)   extra=(--win --x64); setup_wine_path ;;
  esac
  echo "[builder] npx electron-builder ${extra[*]}"
  cd "$ELECTRON_DIR"
  # ELECTRON_BUILDER_EXTRA: extra CLI args (remote build farms pass --config.mac.identity=null
  # to skip Developer ID signing — the cert lives on the dev machine's keychain; mini only ships adhoc packages).
  # Left unquoted so multiple args split on whitespace.
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
  # gen-icons regenerates png/ico/icns from the logo source. iconutil shells
  # out with a /dev/null redirect, which workspace-write sandboxes deny —
  # under those, keep the previously generated icon.icns (logo unchanged).
  if ! python3 "$ROOT/desktop/scripts/gen-icons.py"; then
    if [ -f "$ELECTRON_DIR/assets/icon.icns" ]; then
      echo "[icons] WARNING: 图标再生成失败(iconutil 沙箱受限) — 沿用现有 assets/icon.icns"
    else
      echo "[icons] ERROR: 图标生成失败且无现成 icon.icns" >&2
      exit 1
    fi
  fi
  build_frontend
  run_electron_builder "$p"
  if [ "$p" = "mac" ]; then
    notarize_mac
    cleanup_unpacked
  fi
  echo ""
  echo "=== $p package done ==="
}

# 2026-10-04: remove the mac unpack dir after a successful build. dist/mac-arm64/Starriver.app gets
# indexed by Spotlight as a real app: every build adds another clickable Starriver icon to
# Launchpad/Spotlight, confusing it with the installed copy (the ghost-icon source of this
# investigation), and clicking it bypasses the single-instance lock into a second instance.
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
