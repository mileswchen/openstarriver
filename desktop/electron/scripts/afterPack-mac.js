// electron-builder afterPack hook — modern macOS icon support (macOS 26+).
//
// If desktop/electron/assets/AppIcon.icon exists (an Icon Composer document),
// compile it with actool into Assets.car, copy it into the packaged app and
// set CFBundleIconName so macOS 26+ renders the author-controlled Liquid
// Glass artwork — no system-generated corrective border ("白边").
// Falls back silently to the legacy icns (CFBundleIconFile) when absent.
//
// Notes:
//  - CFBundleIconFile stays: pre-26 macOS keeps using the icns.
//  - Any actool failure logs a warning and leaves the legacy icon in place,
//    so packaging never breaks because of this hook.

const { execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");

module.exports = async function afterPack(context) {
  if (context.electronPlatformName !== "darwin") return;

  const appDir = path.join(context.appOutDir, `${context.packager.appInfo.productFilename}.app`);
  const resourcesDir = path.join(appDir, "Contents", "Resources");
  const iconDoc = path.join(__dirname, "..", "assets", "AppIcon.icon");
  if (!fs.existsSync(iconDoc)) return;

  let outDir;
  try {
    // os.tmpdir() respects $TMPDIR (sandboxed/CI builds redirect it); a
    // hardcoded /tmp breaks packaging where /tmp is not writable.
    outDir = fs.mkdtempSync(path.join(require("os").tmpdir(), "starriver-icon-"));
    execFileSync("xcrun", [
      "actool", "--compile", outDir,
      "--platform", "macosx",
      "--minimum-deployment-target", "12.0",
      "--app-icon", "AppIcon",
      "--output-partial-info-plist", path.join(outDir, "partial.plist"),
      iconDoc,
    ], { stdio: ["ignore", "ignore", "pipe"] });

    const car = path.join(outDir, "Assets.car");
    if (!fs.existsSync(car)) throw new Error("actool produced no Assets.car");
    fs.copyFileSync(car, path.join(resourcesDir, "Assets.car"));

    const plist = path.join(appDir, "Contents", "Info.plist");
    try {
      execFileSync("/usr/libexec/PlistBuddy", ["-c", "Set :CFBundleIconName AppIcon", plist]);
    } catch {
      execFileSync("/usr/libexec/PlistBuddy", ["-c", "Add :CFBundleIconName string AppIcon", plist]);
    }
    console.log(`  • modern icon: ${iconDoc} → Assets.car + CFBundleIconName (macOS 26+ borderless rendering)`);
  } catch (err) {
    // BUG-P21: execFileSync attaches the child's stderr as err.stderr —
    // without it the warning only says "Command failed" and the real actool
    // diagnostic is lost, making icon failures impossible to debug.
    const detail = err && err.stderr ? String(err.stderr).trim() : "";
    console.warn(`  • modern icon skipped: ${err.message || err}${detail ? ` — ${detail}` : ""}`);
  } finally {
    // BUG-P20: cleanup is best-effort — a failing rmSync (EBUSY/EPERM on a
    // locked temp dir) must never reject afterPack and break packaging.
    if (outDir) {
      try { fs.rmSync(outDir, { recursive: true, force: true }); } catch { /* best-effort */ }
    }
  }
};
