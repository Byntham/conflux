#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: macos/build-app.sh <conflux binary> <output dir>" >&2
  exit 1
fi

here=$(cd "$(dirname "$0")" && pwd)
conflux=$1
out=$(mkdir -p "$2" && cd "$2" && pwd)
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$here/../Cargo.toml" | head -1)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

swift build -c release --arch arm64 --package-path "$here"
menu_bar=$(swift build -c release --arch arm64 --package-path "$here" --show-bin-path)/Conflux

swiftc -O "$here/Sources/Conflux/Logo.swift" "$here/icon/main.swift" -o "$work/make-icon"
"$work/make-icon" "$here/../assets/icon.svg" "$work/AppIcon.iconset"

app="$out/Conflux.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Helpers" "$app/Contents/Resources"
cp "$menu_bar" "$app/Contents/MacOS/Conflux"
cp "$conflux" "$app/Contents/Helpers/conflux"
cp "$here/../assets/icon.svg" "$app/Contents/Resources/icon.svg"
iconutil -c icns "$work/AppIcon.iconset" -o "$app/Contents/Resources/AppIcon.icns"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"

codesign --force --sign - "$app/Contents/Helpers/conflux"
codesign --force --sign - "$app"

rm -f "$out/conflux-darwin-aarch64-app.zip"
ditto -c -k --norsrc --noextattr --keepParent "$app" "$out/conflux-darwin-aarch64-app.zip"
echo "Built $app and $out/conflux-darwin-aarch64-app.zip"
