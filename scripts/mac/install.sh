#!/bin/bash
# Installs zenith on this Mac from this checkout:
#   1. builds the window (GPUI), the server, zenith-cli and zenith-mcp (Rust), and the web
#      interface for "Open in Browser" when Node is there,
#   2. assembles zenith.app (scripts/mac/package.sh) and puts it in /Applications,
#   3. runs the server in the background from login (a LaunchAgent on 127.0.0.1:4747),
#   4. opens the app.
# Run it again after changing or pulling the code. Releases (GitHub, lsuite.xyz/zenith) need
# none of this: the app sets up its server itself and updates itself.
# ZENITH_BUNDLE_ID overrides the LaunchAgent's label (default dev.zenith.app), ZENITH_CODE_HOME the
# server's state folder (default ~/.zenith/code), ZENITH_SERVER the server: rust (the default) or
# node (the TypeScript one in code/apps/server, which some providers still need), ZENITH_WEB=0
# skips the web interface.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

# French when the Mac speaks French.
case "$(defaults read -g AppleLocale 2>/dev/null || echo "${LANG:-en}")" in fr*) FR=1 ;; *) FR= ;; esac
t() { if [ -n "$FR" ]; then printf "%s" "$1"; else printf "%s" "$2"; fi; }

LABEL="${ZENITH_BUNDLE_ID:-dev.zenith.app}"
PORT=4747
HOME_DIR="${ZENITH_CODE_HOME:-$HOME/.zenith/code}"
SERVER_KIND="${ZENITH_SERVER:-rust}"
NODE="$(command -v node || true)"
APP_NAME="zenith.app"
if [ -w /Applications ]; then DEST=/Applications; else DEST="$HOME/Applications"; fi
LOGS="$HOME/Library/Logs/Zenith"

say() { printf "\n\033[1;33m✦ %s\033[0m\n" "$1"; }

if ! command -v cargo >/dev/null 2>&1; then
  echo "$(t "zenith demande Rust : installe-le depuis https://rustup.rs puis relance ce script." "zenith needs Rust: install it from https://rustup.rs and run this script again.")" >&2
  exit 1
fi
if ! xcrun --show-sdk-path >/dev/null 2>&1; then
  echo "$(t "zenith demande les outils de Xcode : xcode-select --install" "zenith needs Xcode's tools: xcode-select --install")" >&2
  exit 1
fi
case "$SERVER_KIND" in rust|node) ;; *) echo "ZENITH_SERVER: rust or node" >&2; exit 1 ;; esac
if [ "$SERVER_KIND" = node ] && [ -z "$NODE" ]; then
  echo "$(t "ZENITH_SERVER=node demande Node 22.16+ (ou 24)." "ZENITH_SERVER=node needs Node 22.16+ (or 24).")" >&2
  exit 1
fi
# Something else on the port would keep the server from starting.
if lsof -nP -iTCP:$PORT -sTCP:LISTEN -t >/dev/null 2>&1 && ! launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
  echo "$(t "Le port $PORT est déjà pris. Arrête ce qui l'occupe puis relance ce script." "Port $PORT is taken. Stop whatever holds it and run this script again.")" >&2
  exit 1
fi

say "$(t "1/4 Compilation (fenêtre, serveur, zenith-cli, zenith-mcp)" "1/4 Building (window, server, zenith-cli, zenith-mcp)")"
ZENITH_AGENT_LABEL="$LABEL" ZENITH_CODE_HOME="$HOME_DIR" \
  cargo build --release -p zenith-app -p zenith-code -p zenith-commands
if [ "${ZENITH_WEB:-1}" != 0 ] && [ -n "$NODE" ]; then
  [ -d node_modules ] || npm ci
  npm run code:build || echo "$(t "Interface web non construite (Ouvrir dans le navigateur indisponible)." "Web interface not built (Open in Browser unavailable).")" >&2
fi

say "$(t "2/4 zenith.app" "2/4 zenith.app")"
ZENITH_BUNDLE_ID="$LABEL" bash scripts/mac/package.sh
{ pkill -x zenith; pkill -x Zenith; } 2>/dev/null && sleep 1 || true   # the previous version, if running
rm -rf "$DEST/$APP_NAME" "$DEST/Zénith.app"
ditto dist/zenith.app "$DEST/$APP_NAME"
echo "$(t "Installée dans" "Installed in") $DEST/$APP_NAME"
# zenith-cli and zenith-mcp on the PATH when ~/.local/bin is there.
if [ -d "$HOME/.local/bin" ]; then
  ln -sf "$DEST/$APP_NAME/Contents/MacOS/zenith-cli" "$HOME/.local/bin/zenith-cli"
  ln -sf "$DEST/$APP_NAME/Contents/MacOS/zenith-mcp" "$HOME/.local/bin/zenith-mcp"
fi

say "$(t "3/4 Serveur en arrière-plan (démarre avec la session)" "3/4 Background server (starts at login)")"
mkdir -p "$LOGS" "$HOME_DIR"
if [ "$SERVER_KIND" = rust ]; then
  "$DEST/$APP_NAME/Contents/MacOS/zenith-cli" setup
else
  PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
  mkdir -p "$(dirname "$PLIST")"
  cat > "$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$NODE</string><string>$ROOT/code/apps/server/dist/bin.mjs</string><string>serve</string>
    <string>--host</string><string>127.0.0.1</string><string>--port</string><string>$PORT</string>
    <string>--base-dir</string><string>$HOME_DIR</string>
  </array>
  <key>WorkingDirectory</key><string>$ROOT</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>$(dirname "$NODE"):$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
    <key>NODE_ENV</key><string>production</string>
    <key>ZENITH_NO_STARTUP_TOKEN</key><string>1</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>$LOGS/server.log</string>
  <key>StandardErrorPath</key><string>$LOGS/server.log</string>
</dict>
</plist>
PLIST_EOF
  launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
  for _ in $(seq 1 20); do launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || break; sleep 0.5; done
  launchctl bootstrap "gui/$(id -u)" "$PLIST"
  launchctl kickstart -k "gui/$(id -u)/$LABEL"
fi

say "$(t "4/4 Ouverture" "4/4 Opening")"
for _ in $(seq 1 30); do
  curl -fs --max-time 2 "http://127.0.0.1:$PORT/.well-known/t3/environment" >/dev/null 2>&1 && break
  sleep 1
done
open "$DEST/$APP_NAME"
echo "$(t "zenith est prêt. zenith-cli et zenith-mcp sont dans $DEST/$APP_NAME/Contents/MacOS." "zenith is ready. zenith-cli and zenith-mcp are in $DEST/$APP_NAME/Contents/MacOS.")"
