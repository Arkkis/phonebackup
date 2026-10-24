#!/bin/sh
# Builds Phone-Backup-x86_64.AppImage in this directory.
set -e
cd "$(dirname "$0")"
cargo build --release --locked --manifest-path ../Cargo.toml

tool=appimagetool-x86_64.AppImage
[ -x "$tool" ] || { curl -fLo "$tool" https://github.com/AppImage/appimagetool/releases/download/continuous/$tool; chmod +x "$tool"; }

rm -rf AppDir
install -Dm755 ../target/release/phonebackup AppDir/usr/bin/phonebackup
install -Dm644 phonebackup.svg AppDir/phonebackup.svg
install -Dm644 phonebackup.desktop AppDir/phonebackup.desktop
ln -s usr/bin/phonebackup AppDir/AppRun

ARCH=x86_64 ./"$tool" --appimage-extract-and-run AppDir Phone-Backup-x86_64.AppImage
