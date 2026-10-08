#!/bin/bash
# Build the corplink-gui menu-bar app and wrap it into a DMG.
#
# The app is ad-hoc signed (no Apple Developer ID): it runs fine on the
# machine it was built on; elsewhere the first open needs
# System Settings -> Privacy & Security -> "Open Anyway".
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
APP_NAME="Corplink GUI"
BUNDLE_ID="com.github.pinkd.corplink-rs.gui"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "${REPO_ROOT}/gui/Cargo.toml" | head -1)"
BIN="${REPO_ROOT}/target/release/corplink-gui"
DIST="${REPO_ROOT}/dist"
APP="${DIST}/${APP_NAME}.app"
DMG="${DIST}/corplink-gui-${VERSION}.dmg"

echo "==> building corplink-gui ${VERSION}"
cargo build --release -p corplink-gui
[[ -x "${BIN}" ]] || { echo "binary missing: ${BIN}" >&2; exit 1; }

echo "==> assembling ${APP}"
rm -rf "${APP}"
mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources"
/bin/cp "${BIN}" "${APP}/Contents/MacOS/corplink-gui"

cat > "${APP}/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Corplink GUI</string>
    <key>CFBundleDisplayName</key>
    <string>Corplink GUI</string>
    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID}</string>
    <key>CFBundleExecutable</key>
    <string>corplink-gui</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleIconFile</key>
    <string>app.icns</string>
    <key>LSUIElement</key>
    <true/>
    <!-- with its window hidden the app only polls the daemon and the menu
         on a timer, which App Nap would stretch to minutes -->
    <key>NSAppSleepDisabled</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
</dict>
</plist>
EOF

# a simple template icon, drawn with plain python so no extra tools are needed
python3 - "${APP}/Contents/Resources" <<'PYEOF'
import struct, sys, zlib, os

def png(path, size, rgb):
    # a rounded square with a darker border, alpha-masked
    half = size * 0.46
    corner = size * 0.18

    def inside(ax, ay, inset):
        # within the rounded square shrunk by `inset` pixels
        h, c = half - inset, corner - inset
        qx, qy = max(ax - (h - c), 0), max(ay - (h - c), 0)
        return (qx * qx + qy * qy) ** 0.5 <= c

    center = (size - 1) / 2
    dark = bytes(int(c * 0.55) for c in rgb)
    raw = b""
    for y in range(size):
        row = b"\x00"  # filter type none
        for x in range(size):
            ax, ay = abs(x - center), abs(y - center)
            if not inside(ax, ay, 0):
                row += bytes(4)
            elif inside(ax, ay, 2):
                row += bytes(rgb) + b"\xff"
            else:
                row += dark + b"\xff"
        raw += row

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", ihdr))
        f.write(chunk(b"IDAT", zlib.compress(raw, 9)))
        f.write(chunk(b"IEND", b""))

out = sys.argv[1]
# the "connected" green
png(os.path.join(out, "icon-128.png"), 128, (48, 209, 88))
PYEOF
if /usr/bin/sips -s format icns "${APP}/Contents/Resources/icon-128.png" --out "${APP}/Contents/Resources/app.icns" >/dev/null 2>&1; then
    rm -f "${APP}/Contents/Resources/icon-128.png"
else
    echo "warning: could not convert the icon to icns; the app will use the default icon" >&2
    rm -f "${APP}/Contents/Resources/icon-128.png" "${APP}/Contents/Resources/app.icns"
    /usr/bin/sed -i '' '/CFBundleIconFile/,+1d' "${APP}/Contents/Info.plist"
fi

echo "==> ad-hoc signing"
/usr/bin/codesign --force --sign - "${APP}"

echo "==> creating ${DMG}"
# the app next to a link to /Applications, to drag it there
STAGE="$(/usr/bin/mktemp -d)"
trap '/bin/rm -rf "${STAGE}"' EXIT
/usr/bin/ditto "${APP}" "${STAGE}/${APP_NAME}.app"
/bin/ln -s /Applications "${STAGE}/Applications"
rm -f "${DMG}"
/usr/bin/hdiutil create -volname "Corplink GUI" -srcfolder "${STAGE}" -ov -format UDZO "${DMG}" >/dev/null

echo "done: ${DMG}"
echo "first open on another machine: right-click the app or use System Settings -> Privacy & Security -> Open Anyway (no Developer ID)"
