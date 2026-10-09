#!/bin/bash
# Build the corplink-gui menu-bar app and wrap it into a DMG.
#
# The app carries the daemon and the launchd install scripts, so it alone
# installs and runs the service: its setup window writes the config and
# installs the service behind the administrator prompt.
#
# The app is ad-hoc signed (no Apple Developer ID): it runs fine on the
# machine it was built on; elsewhere the first open needs
# System Settings -> Privacy & Security -> "Open Anyway". It is built for
# this Mac's architecture only.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
APP_NAME="Corplink GUI"
BUNDLE_ID="com.github.pinkd.corplink-rs.gui"
LABEL="com.github.pinkd.corplink-rs"
crate_version() {
    sed -n 's/^version = "\(.*\)"/\1/p' "$1" | head -1
}
VERSION="$(crate_version "${REPO_ROOT}/gui/Cargo.toml")"
DAEMON_VERSION="$(crate_version "${REPO_ROOT}/Cargo.toml")"
BIN="${REPO_ROOT}/target/release/corplink-gui"
DAEMON_BIN="${REPO_ROOT}/target/release/corplink-rs"
DIST="${REPO_ROOT}/dist"
APP="${DIST}/${APP_NAME}.app"
DMG="${DIST}/corplink-gui-${VERSION}.dmg"

# the GUI tells a stale daemon by its version, so both must be the same
[[ "${VERSION}" == "${DAEMON_VERSION}" ]] || {
    echo "version mismatch: corplink-gui ${VERSION}, corplink-rs ${DAEMON_VERSION}" >&2
    exit 1
}

cd "${REPO_ROOT}"
if [[ ! -f libwg/libwg.a ]]; then
    echo "==> building libwg"
    ./libwg/build.sh
fi
echo "==> building corplink-rs and corplink-gui ${VERSION}"
cargo build --release -p corplink-rs -p corplink-gui
for binary in "${BIN}" "${DAEMON_BIN}"; do
    [[ -x "${binary}" ]] || { echo "binary missing: ${binary}" >&2; exit 1; }
done

echo "==> assembling ${APP}"
rm -rf "${APP}"
mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources/scripts" \
    "${APP}/Contents/Resources/launchd"
/bin/cp "${BIN}" "${APP}/Contents/MacOS/corplink-gui"
# the service the setup window installs, with the command line install's
# scripts: they find the plist template at ../launchd from their directory
/bin/cp "${DAEMON_BIN}" "${APP}/Contents/MacOS/corplink-rs"
/bin/cp scripts/install-launchd.sh scripts/uninstall-launchd.sh \
    "${APP}/Contents/Resources/scripts/"
/bin/cp "launchd/${LABEL}.plist" "${APP}/Contents/Resources/launchd/"

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

# a minimal icon to match the menu bar: a white rounded square with the
# tray icon's padlock on it, drawn with plain python so no extra tools are
# needed and it stays in step with gui/src/tray.rs
python3 - "${APP}/Contents/Resources" <<'PYICON'
import math, struct, sys, zlib

def write_png(path, size, pixels):
    raw = b""
    for y in range(size):
        raw += b"\x00" + bytes(v for px in pixels[y*size:(y+1)*size] for v in px)
    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)))
        f.write(chunk(b"IDAT", zlib.compress(raw, 9)))
        f.write(chunk(b"IEND", b""))

# the padlock of gui/src/tray.rs, in the 44-unit space of the tray icon
def rrect_sdf(x, y, cx, cy, hx, hy, r):
    qx = abs(x - cx) - (hx - r); qy = abs(y - cy) - (hy - r)
    return math.hypot(max(qx, 0), max(qy, 0)) + min(max(qx, qy), 0) - r

def lock_dist(x, y):
    best = abs(rrect_sdf(x, y, 0.0, 6.7, 15.0, 11.2, 4.6))
    if y <= -2.0:
        best = min(best, abs(math.hypot(x, y + 3.8) - 7.0))
    return best

size = 128
c = (size - 1) / 2
scale = size / 58.0   # the glyph fills the square
px = []
for y in range(size):
    for x in range(size):
        bg = border = lock = fill = 0.0
        for sy in range(3):
            for sx in range(3):
                X = x + (sx + 0.5) / 3; Y = y + (sy + 0.5) / 3
                sd = rrect_sdf(X, Y, c, c, c - 1, c - 1, size * 0.185)
                if sd <= 0:
                    bg += 1 / 9
                    if abs(sd) <= 1.4:
                        border += 1 / 9
                    gx = (X - c) / scale; gy = (Y - c) / scale - 1.0
                    if lock_dist(gx, gy) <= 1.6:
                        lock += 1 / 9
                    if rrect_sdf(gx, gy, 0.0, 6.7, 15.0, 11.2, 4.6) <= -1.7:
                        fill += 1 / 9
        if bg <= 0:
            px.append((0, 0, 0, 0)); continue
        col = (252, 252, 253)
        if border > 0:
            t = border / bg
            col = tuple(round(col[i] + ((210, 210, 215)[i] - col[i]) * t) for i in range(3))
        if fill > 0:
            # the body interior in the "connected" green, like the tray icon
            t = fill / bg
            col = tuple(round(col[i] + ((50, 199, 94)[i] - col[i]) * t) for i in range(3))
        if lock > 0:
            t = lock / bg
            col = tuple(round(col[i] + ((46, 46, 51)[i] - col[i]) * t) for i in range(3))
        px.append((col[0], col[1], col[2], round(bg * 255)))

write_png(sys.argv[1] + "/icon-128.png", size, px)
PYICON
if /usr/bin/sips -s format icns "${APP}/Contents/Resources/icon-128.png" --out "${APP}/Contents/Resources/app.icns" >/dev/null 2>&1; then
    rm -f "${APP}/Contents/Resources/icon-128.png"
else
    echo "warning: could not convert the icon to icns; the app will use the default icon" >&2
    rm -f "${APP}/Contents/Resources/icon-128.png" "${APP}/Contents/Resources/app.icns"
    /usr/bin/sed -i '' '/CFBundleIconFile/,+1d' "${APP}/Contents/Info.plist"
fi

echo "==> ad-hoc signing"
# nested code first, the app's seal covers it
/usr/bin/codesign --force --sign - "${APP}/Contents/MacOS/corplink-rs"
/usr/bin/codesign --force --sign - "${APP}"
/usr/bin/codesign --verify --deep --strict "${APP}"

echo "==> creating ${DMG}"
# the app next to a link to /Applications, to drag it there
STAGE="$(/usr/bin/mktemp -d)"
trap '/bin/rm -rf "${STAGE}"' EXIT
/usr/bin/ditto "${APP}" "${STAGE}/${APP_NAME}.app"
/bin/ln -s /Applications "${STAGE}/Applications"
rm -f "${DMG}"
/usr/bin/hdiutil create -volname "Corplink GUI" -srcfolder "${STAGE}" -ov -format UDZO "${DMG}" >/dev/null

echo "done: ${DMG}"
echo "first open on another Mac: System Settings -> Privacy & Security -> Open Anyway (no Developer ID); built for $(uname -m) Macs only"
