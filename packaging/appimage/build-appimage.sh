#!/bin/bash
set -euo pipefail

# Build a GUI-only AppImage for ButteredDASD
# Usage: build-appimage.sh VERSION
# Example: build-appimage.sh 0.5.0

VERSION="${1:?Usage: build-appimage.sh VERSION}"
APPDIR="ButteredDASD.AppDir"

# Clean previous build
rm -rf "$APPDIR" build-appimage

# Build GUI with /usr prefix (AppImage convention)
cmake -B build-appimage \
    -DCMAKE_INSTALL_PREFIX=/usr \
    -DCMAKE_BUILD_TYPE=Release \
    -DBUILD_GUI=ON \
    -DBUILD_INDEXER=OFF
cmake --build build-appimage

# Install into AppDir
DESTDIR="$APPDIR" cmake --install build-appimage

# Download linuxdeploy + Qt plugin if not already present
if [[ ! -x linuxdeploy-x86_64.AppImage ]]; then
    wget -q "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage"
    chmod +x linuxdeploy-x86_64.AppImage
fi
if [[ ! -x linuxdeploy-plugin-qt-x86_64.AppImage ]]; then
    wget -q "https://github.com/linuxdeploy/linuxdeploy-plugin-qt/releases/download/continuous/linuxdeploy-plugin-qt-x86_64.AppImage"
    chmod +x linuxdeploy-plugin-qt-x86_64.AppImage
fi

# Detect Qt6 qmake location
if command -v qmake6 &>/dev/null; then
    export QMAKE=qmake6
elif [[ -x /usr/lib/qt6/bin/qmake ]]; then
    export QMAKE=/usr/lib/qt6/bin/qmake
else
    export QMAKE=qmake
fi

export VERSION

# Bundle all dependencies and produce AppImage
./linuxdeploy-x86_64.AppImage \
    --appdir "$APPDIR" \
    --desktop-file "$APPDIR/usr/share/applications/org.theboscoclub.btrdasd-gui.desktop" \
    --icon-file "$APPDIR/usr/share/icons/hicolor/scalable/apps/btrdasd-gui.svg" \
    --plugin qt \
    --output appimage

echo "Built: ButteredDASD-${VERSION}-x86_64.AppImage"
