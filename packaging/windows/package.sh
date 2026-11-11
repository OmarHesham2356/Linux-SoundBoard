#!/usr/bin/env bash
# Assembles a relocatable Windows build from the MSYS2 MINGW64 tree.
#
# Usage (inside an MSYS2 MINGW64 shell):
#   bash packaging/windows/package.sh
#
# Output:
#   dist/windows/                     portable application directory
#   dist/LinuxSoundboard-windows-x64.zip

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
DIST="$REPO_ROOT/dist/windows"
APP_EXE="$REPO_ROOT/target/release/linux-soundboard.exe"
MINGW_ROOT="${MINGW_PREFIX:-/mingw64}"

if [ ! -f "$APP_EXE" ]; then
    echo "error: $APP_EXE is missing; run cargo build --release first" >&2
    exit 1
fi

rm -rf "$DIST"
mkdir -p "$DIST" "$DIST/share/glib-2.0" "$DIST/share/icons" "$DIST/lib"
cp "$APP_EXE" "$DIST/"

# Copy every MinGW runtime DLL the executable and its dependencies need.
copy_dependencies() {
    local binary="$1"
    while IFS= read -r dll; do
        [ -n "$dll" ] || continue
        local base
        base="$(basename "$dll")"
        if [ ! -f "$DIST/$base" ]; then
            cp "$dll" "$DIST/"
            copy_dependencies "$dll"
        fi
    done < <(ldd "$binary" 2>/dev/null | awk '/=> \/mingw/ {print $3}' | sort -u)
}

copy_dependencies "$DIST/linux-soundboard.exe"

# GSettings schemas are needed by GTK and libadwaita at runtime.
if [ -d "$MINGW_ROOT/share/glib-2.0/schemas" ]; then
    cp -r "$MINGW_ROOT/share/glib-2.0/schemas" "$DIST/share/glib-2.0/"
fi

# Image loaders for gdk-pixbuf.
if [ -d "$MINGW_ROOT/lib/gdk-pixbuf-2.0" ]; then
    mkdir -p "$DIST/lib"
    cp -r "$MINGW_ROOT/lib/gdk-pixbuf-2.0" "$DIST/lib/"
    if [ -f "$DIST/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache" ]; then
        # Rewrite the absolute build-machine paths to the portable layout.
        sed -i "s|$MINGW_ROOT|.|g" "$DIST/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache" || true
    fi
fi

# Icon themes used by the GTK and libadwaita widgets.
for theme in Adwaita hicolor; do
    if [ -d "$MINGW_ROOT/share/icons/$theme" ]; then
        cp -r "$MINGW_ROOT/share/icons/$theme" "$DIST/share/icons/"
    fi
done

# Application icon for shortcuts and the taskbar.
ICON_PNG="$REPO_ROOT/resources/icons/256x256/apps/com.linuxsoundboard.app.png"
if [ -f "$ICON_PNG" ]; then
    cp "$ICON_PNG" "$DIST/linux-soundboard.png"
fi

cp "$REPO_ROOT/LICENSE" "$DIST/LICENSE" 2>/dev/null || true
cp "$REPO_ROOT/NOTICE.md" "$DIST/NOTICE.md" 2>/dev/null || true

VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO_ROOT/src/Cargo.toml" | head -1)"
printf '%s\n' "$VERSION" > "$DIST/version.txt"

# A plain zip for users who do not want the installer.
if command -v zip >/dev/null 2>&1; then
    (cd "$DIST" && zip -qr "$REPO_ROOT/dist/LinuxSoundboard-$VERSION-windows-x64-portable.zip" .)
fi

echo "Windows package ready in $DIST (version $VERSION)"
