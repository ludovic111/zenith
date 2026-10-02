#!/usr/bin/env bash
# Builds dist/zenith.app from the release binaries and zips it as dist/zenith-macos-<arch>.zip,
# the asset the in-app updater downloads. The bundle holds the window (zenith), the server
# (zenith-code), zenith-cli, zenith-mcp and, when built, the web interface (Resources/web, for
# "Open in Browser"). Signing: Developer ID and notarization when scripts/mac/prepare-signing.sh
# set APPLE_SIGNING_IDENTITY (and the API key), ad hoc otherwise. ZENITH_BUNDLE_ID overrides the
# bundle identifier (default dev.zenith.app).
set -euo pipefail
cd "$(dirname "$0")/../.."
version=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
case "$(uname -m)" in
  arm64|aarch64) arch=arm64 ;;
  x86_64) arch=x86_64 ;;
  *) echo "Unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
binaries=(zenith zenith-code zenith-cli zenith-mcp)
for binary in "${binaries[@]}"; do
  test -x "target/release/$binary" || { echo "Missing target/release/$binary: cargo build --release -p zenith-app -p zenith-code -p zenith-commands" >&2; exit 1; }
  actual=$("target/release/$binary" --version | head -1)
  test "$actual" = "$binary $version" || { echo "$binary reports \"$actual\", expected \"$binary $version\"" >&2; exit 1; }
done
bundle=dist/zenith.app
rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
for binary in "${binaries[@]}"; do
  cp "target/release/$binary" "$bundle/Contents/MacOS/$binary"
done
sed -e "s|<string>0\.0\.0</string>|<string>$version</string>|g" \
    -e "s|<string>dev\.zenith\.app</string>|<string>${ZENITH_BUNDLE_ID:-dev.zenith.app}</string>|" \
    scripts/mac/Info.plist > "$bundle/Contents/Info.plist"
plutil -lint "$bundle/Contents/Info.plist" >/dev/null
cp crates/zenith-app/icons/AppIcon.icns "$bundle/Contents/Resources/AppIcon.icns"
for web in code/apps/server/dist/client code/apps/web/dist; do
  if [ -f "$web/index.html" ]; then
    cp -R "$web" "$bundle/Contents/Resources/web"
    break
  fi
done
if [ -n "${APPLE_SIGNING_IDENTITY:-}" ]; then
  # Developer ID: hardened runtime, secure timestamp, the helpers first and the bundle last.
  for binary in zenith-code zenith-cli zenith-mcp; do
    codesign --force --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$bundle/Contents/MacOS/$binary"
  done
  codesign --force --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$bundle"
  codesign --verify --deep --strict "$bundle"
  if [ -n "${APPLE_API_KEY_PATH:-}" ]; then
    submission="dist/zenith-notarize-$arch.zip"
    rm -f "$submission"
    ditto -c -k --keepParent "$bundle" "$submission"
    result=$(xcrun notarytool submit "$submission" --key "$APPLE_API_KEY_PATH" \
      --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER" --wait --output-format json)
    rm -f "$submission"
    echo "$result"
    status=$(printf '%s' "$result" | python3 -c 'import json, sys; print(json.load(sys.stdin).get("status", ""))')
    if [ "$status" != Accepted ]; then
      id=$(printf '%s' "$result" | python3 -c 'import json, sys; print(json.load(sys.stdin).get("id", ""))')
      test -n "$id" && xcrun notarytool log "$id" --key "$APPLE_API_KEY_PATH" \
        --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER" || true
      echo "Notarization was not accepted: $status" >&2
      exit 1
    fi
    xcrun stapler staple "$bundle"
    xcrun stapler validate "$bundle"
    spctl --assess --type execute --verbose=2 "$bundle"
  fi
else
  codesign --force --deep --sign - "$bundle"
  codesign --verify --deep --strict "$bundle"
fi
rm -f "dist/zenith-macos-$arch.zip"
ditto -c -k --sequesterRsrc --keepParent "$bundle" "dist/zenith-macos-$arch.zip"
echo "Built $bundle ($version, $arch) and dist/zenith-macos-$arch.zip"
