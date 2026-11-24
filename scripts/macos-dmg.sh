#!/bin/sh
# Builds target/dmg/sdb-<version>-<arch>.dmg: Simple Database.app (the desktop app, with
# the sdb CLI inside) and a link to /Applications to drag it onto. macOS only.
set -eu

cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
arch=$(uname -m)
out=target/dmg
app="$out/root/Simple Database.app"

cargo build --release

rm -rf "$out"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/sdb-gui target/release/sdb "$app/Contents/MacOS/"
cp assets/icon/sdb.icns "$app/Contents/Resources/sdb.icns"
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Simple Database</string>
  <key>CFBundleDisplayName</key><string>Simple Database</string>
  <key>CFBundleIdentifier</key><string>dev.sdb.sdb-gui</string>
  <key>CFBundleExecutable</key><string>sdb-gui</string>
  <key>CFBundleIconFile</key><string>sdb</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
# An ad-hoc signature, so Apple silicon runs it; not notarized.
codesign --force --deep --sign - "$app"
ln -s /Applications "$out/root/Applications"

dmg="$out/sdb-$version-$arch.dmg"
hdiutil create -volname "Simple Database $version" -srcfolder "$out/root" -ov -format UDZO "$dmg" >/dev/null
rm -rf "$out/root"
echo "$dmg"
