#!/usr/bin/env bash
# Builds build/appimage/UltraDisplay-x86_64.AppImage from this tree.
# Tools (pinned + sha256-verified) are cached in .toolchain/appimage/.
set -euo pipefail
. "$(dirname "$0")/env.sh"

TOOLS="$UD_TOOLCHAIN/appimage"
OUT="$UD_ROOT/build/appimage"
APPDIR="$OUT/AppDir"
SRC="$UD_ROOT/packaging/appimage"

APPIMAGETOOL_URL=https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage
APPIMAGETOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
RUNTIME_URL=https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64
RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d

fetch() { # url sha256 dest
    if [ ! -f "$3" ] || ! echo "$2  $3" | sha256sum -c --status -; then
        echo "downloading $(basename "$3")"
        curl -fsSL -o "$3.part" "$1"
        echo "$2  $3.part" | sha256sum -c --status - || { echo "checksum mismatch: $1" >&2; exit 1; }
        mv "$3.part" "$3"
    fi
}

mkdir -p "$TOOLS" "$OUT"
fetch "$APPIMAGETOOL_URL" "$APPIMAGETOOL_SHA256" "$TOOLS/appimagetool-x86_64.AppImage"
fetch "$RUNTIME_URL" "$RUNTIME_SHA256" "$TOOLS/runtime-x86_64"
chmod +x "$TOOLS/appimagetool-x86_64.AppImage"

(cd "$UD_ROOT/host" && cargo build --release --locked -p ultradisplayd -p ultradisplay)

rm -rf "$APPDIR"
install -Dm755 "$UD_ROOT/host/target/release/ultradisplay" "$APPDIR/usr/bin/ultradisplay"
install -Dm755 "$UD_ROOT/host/target/release/ultradisplayd" "$APPDIR/usr/bin/ultradisplayd"
install -Dm755 "$SRC/AppRun" "$APPDIR/AppRun"
install -Dm644 "$SRC/ultradisplay.desktop" "$APPDIR/ultradisplay.desktop"
install -Dm644 "$SRC/ultradisplay.svg" "$APPDIR/ultradisplay.svg"
install -Dm644 "$UD_ROOT/packaging/ultradisplay/config.example.toml" \
    "$APPDIR/usr/share/doc/ultradisplay/config.example.toml"
# Not bundled on purpose: libpipewire must match the running PipeWire, NVENC/CUDA/EGL come
# from the NVIDIA driver, and GTK4/libadwaita are used from the (Arch) system so the app
# follows the system theme.

# Build next to the target and rename into place: works while the old AppImage is running
# (the running copy keeps its inode).
ARCH=x86_64 "$TOOLS/appimagetool-x86_64.AppImage" --no-appstream \
    --runtime-file "$TOOLS/runtime-x86_64" \
    "$APPDIR" "$OUT/.UltraDisplay-x86_64.AppImage.new"
mv -f "$OUT/.UltraDisplay-x86_64.AppImage.new" "$OUT/UltraDisplay-x86_64.AppImage"
echo "built $OUT/UltraDisplay-x86_64.AppImage"
