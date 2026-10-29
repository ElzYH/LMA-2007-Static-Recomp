#!/bin/bash
# tools/macos/bundle.sh <release folder> [out dir] [version]: wraps a release (lma-app, lma-cli,
# assets/, mods/) as LMA.app, a double-clickable macOS game app (the maintainer, 22 Oct: "a normal game app,
# a shortcut on the desktop and everything runs").
#
#   LMA.app/Contents/MacOS/lma-app (+ lma-cli)   the binaries
#   LMA.app/Contents/Resources/mods, assets      the release's mod spaces and glyph sets
#   LMA.app/Contents/Resources/source, ...       the first run's source and FFmpeg's licence, when
#                                                the release folder has them
#   LMA.app/Contents/Resources/LMA.icns          only with LMA_BUNDLE_ICON=<1024 px png>; a release has
#                                                none (the maintainer, 28 Oct: only the game's own icon, made on the
#                                                player's Mac from their default.xex)
#
# The game keeps its saves, states and userdata in ~/Library/Application Support/lma-rs (the
# bundle stays read-only; lma-app/src/bundle.rs). No game files go into the bundle: the first run
# asks for the player's own game. Signed ad hoc (codesign -s -), so it opens on the Mac that built
# it; a Developer ID signature and notarisation come with a public release.
set -euo pipefail
src=${1:?usage: bundle.sh <release folder> [out dir] [version]}
out=${2:-.}
version=${3:-1.0}
repo=$(cd "$(dirname "$0")/../.." && pwd)
[ -x "$src/lma-app" ] || { echo "no lma-app in $src" >&2; exit 1; }
# a binary linked against Homebrew's FFmpeg only starts on a Mac with that FFmpeg installed: dyld
# stops it before main, with no message (mac3's #833 review). A bundle for other Macs needs the
# static XMA-only FFmpeg (FFMPEG_STATIC=1, tools/ffmpeg_min.sh, #827); LMA_BUNDLE_LOCAL=1 makes one
# for this Mac anyway.
brew_libs=$(otool -L "$src/lma-app" | awk '/\/opt\/homebrew\/|\/usr\/local\//{print $1}')
if [ -n "$brew_libs" ]; then
  if [ "${LMA_BUNDLE_LOCAL:-0}" = 1 ]; then
    echo "note: lma-app links Homebrew libraries; this LMA.app runs only where they are installed:" >&2
  else
    echo "lma-app links Homebrew libraries, so LMA.app wouldn't start on a Mac without them:" >&2
    echo "$brew_libs" | sed 's/^/  /' >&2
    echo "build it with FFMPEG_STATIC=1 (tools/ffmpeg_min.sh), or set LMA_BUNDLE_LOCAL=1 for this Mac only" >&2
    exit 1
  fi
  echo "$brew_libs" | sed 's/^/  /' >&2
fi
app="$out/LMA.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$src/lma-app" "$app/Contents/MacOS/"
[ -x "$src/lma-cli" ] && cp "$src/lma-cli" "$app/Contents/MacOS/"
# source/ is what the first run builds from (prepare.rs looks in ../Resources/source); FFmpeg's
# licence and relink recipe go with the statically linked FFmpeg (package_release --ffmpeg-static)
for d in mods assets source FFmpeg-relink; do
  [ -d "$src/$d" ] && cp -R "$src/$d" "$app/Contents/Resources/"
done
for f in LICENSE-FFmpeg.txt README-mac.txt; do
  [ -f "$src/$f" ] && cp "$src/$f" "$app/Contents/Resources/"
done
# the release's mods/ is what tells lma-app it runs from a bundle (bundle.rs): keep one, even empty
mkdir -p "$app/Contents/Resources/mods"

# the icon, when given (LMA_BUNDLE_ICON): every size iconutil wants, from a 1024 px master
icon_key=""
if [ -n "${LMA_BUNDLE_ICON:-}" ]; then
  set_dir=$(mktemp -d)/LMA.iconset
  mkdir -p "$set_dir"
  for s in 16 32 128 256 512; do
    sips -z $s $s "$LMA_BUNDLE_ICON" --out "$set_dir/icon_${s}x${s}.png" >/dev/null
    sips -z $((s * 2)) $((s * 2)) "$LMA_BUNDLE_ICON" --out "$set_dir/icon_${s}x${s}@2x.png" >/dev/null
  done
  iconutil -c icns "$set_dir" -o "$app/Contents/Resources/LMA.icns"
  rm -rf "$(dirname "$set_dir")"
  icon_key="<key>CFBundleIconFile</key><string>LMA</string>"
fi

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>LMA</string>
  <key>CFBundleDisplayName</key><string>LMA Manager 2007</string>
  <key>CFBundleIdentifier</key><string>com.lma-rs.lma</string>
  <key>CFBundleExecutable</key><string>lma-app</string>
  $icon_key
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.sports-games</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
codesign --force --deep -s - "$app" 2>/dev/null || echo "codesign failed: the app won't open until it is signed" >&2
# copied to another Mac (a download or a zip), an ad hoc signed app is quarantined and macOS 15 no
# longer offers Open in the right-click menu: System Settings > Privacy & Security > Open Anyway,
# or: xattr -dr com.apple.quarantine LMA.app
echo "$app ($(du -sh "$app" | cut -f1))"
