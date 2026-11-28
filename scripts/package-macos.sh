#!/usr/bin/env bash
# Assembles dist/chatty-<version>-macos-arm64.zip from a release build:
#   chatty.app/Contents/MacOS/chatty
#   chatty.app/Contents/Frameworks/   libtranscribe + ggml dylibs, llama/
#   chatty.app/Contents/Resources/    AppIcon.icns, licenses, README
# The bundle is ad-hoc signed with an explicit designated requirement (the
# bundle identifier). Ad-hoc signing alone pins the requirement to the
# binary's hash, so every rebuild lost the Accessibility and Microphone
# grants and macOS asked again on each launch.
set -euo pipefail
cd "$(dirname "$0")/.."
root=$PWD
version=$(sed -nE 's/^version *= *"([^"]+)".*/\1/p' Cargo.toml | head -1)
release=$root/target/release
out=$root/dist
app=$out/chatty.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks/llama" "$app/Contents/Resources/licenses"

cp "$release/chatty" "$app/Contents/MacOS/chatty"
cp "$root"/native/transcribe/lib/*.dylib "$app/Contents/Frameworks/"
cp -P "$root"/native/llama/*.dylib "$app/Contents/Frameworks/llama/"
cp "$root/native/llama/LICENSE" "$app/Contents/Resources/licenses/llama.cpp.txt"
cp -R "$root/native/transcribe/licenses" "$app/Contents/Resources/licenses/transcribe.cpp"
cp "$root/LICENSE" "$app/Contents/Resources/licenses/chatty.txt"
cp "$root/THIRD-PARTY.md" "$app/Contents/Resources/licenses/"
cp "$root/README.md" "$app/Contents/Resources/"

# AppIcon.icns from the 512 px app icon (the art of assets/chatty.ico).
iconset=$out/AppIcon.iconset
rm -rf "$iconset"
mkdir -p "$iconset"
for n in 16 32 128 256 512; do
  sips -z $n $n "$root/assets/icon-512.png" --out "$iconset/icon_${n}x${n}.png" >/dev/null
  if [ $n -le 256 ]; then
    sips -z $((n * 2)) $((n * 2)) "$root/assets/icon-512.png" --out "$iconset/icon_${n}x${n}@2x.png" >/dev/null
  fi
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
rm -rf "$iconset"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>chatty</string>
  <key>CFBundleDisplayName</key><string>chatty</string>
  <key>CFBundleIdentifier</key><string>com.github.fantasyflip.chatty</string>
  <key>CFBundleExecutable</key><string>chatty</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>13.3</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key><string>chatty records while you hold the trigger and transcribes on this Mac.</string>
</dict>
</plist>
PLIST

# Inside out: libraries first, then the bundle.
find "$app/Contents/Frameworks" -name '*.dylib' -type f -exec codesign --force --sign - {} \;
codesign --force --sign - --identifier com.github.fantasyflip.chatty \
  --requirements '=designated => identifier "com.github.fantasyflip.chatty"' "$app"
codesign --verify --deep --strict "$app"

zip=$out/chatty-$version-macos-arm64.zip
rm -f "$zip"
(cd "$out" && ditto -c -k --keepParent chatty.app "$zip")
exe=$(stat -f %z "$app/Contents/MacOS/chatty")
echo "chatty $((exe / 1048576)) MB, zip $(( $(stat -f %z "$zip") / 1048576 )) MB -> $zip"
