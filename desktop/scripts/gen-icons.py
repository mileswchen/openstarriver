#!/usr/bin/env python3
"""Icon pipeline entry point — regenerates every icon from the design source.

Layout since the 2026-10 rework + Linux hicolor fix:

  make-logo.py   design source. Draws the Starriver icon (black background,
                 four-pointed star over a river of starlight) and writes
                 assets/icon.png (1024), icon.ico (16..256), icon.icns
                 (16..1024, macOS) and icon.svg (vector reference).
                 Deterministic — same bytes on every run.

  this shim      1. runs make-logo.py, then
                 2. regenerates assets/icons/<n>x<n>.png (16..512) from the
                    fresh icon.png for the Linux deb: electron-builder installs
                    each size-named file into
                    /usr/share/icons/hicolor/<w>x<h>/apps/<name>.png — a single
                    icon.png lands in hicolor/0x0/, which no desktop
                    environment scans, leaving the Applications entry
                    iconless. Regenerated on every run so design updates
                    propagate.

package.sh calls this on every run; all outputs are byte-reproducible.
"""
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ASSETS = os.path.join(HERE, "..", "electron", "assets")
PNG_PATH = os.path.join(ASSETS, "icon.png")
ICONS_DIR = os.path.join(ASSETS, "icons")
LINUX_SIZES = [16, 32, 48, 64, 128, 256, 512]


def linux_icon_set() -> None:
    """Size-named PNGs for the Linux deb (see module docstring)."""
    os.makedirs(ICONS_DIR, exist_ok=True)
    from PIL import Image

    source = Image.open(PNG_PATH).convert("RGBA")
    for size in LINUX_SIZES:
        source.resize((size, size), Image.Resampling.LANCZOS).save(
            os.path.join(ICONS_DIR, f"{size}x{size}.png")
        )
    print(f"  generated {len(LINUX_SIZES)} sizes in {ICONS_DIR}")


def main() -> int:
    rc = subprocess.call([sys.executable, os.path.join(HERE, "make-logo.py")])
    if rc != 0:
        return rc
    if not os.path.exists(PNG_PATH):
        print(f"ERROR: {PNG_PATH} missing after make-logo.py")
        return 1
    linux_icon_set()
    return 0


if __name__ == "__main__":
    sys.exit(main())
