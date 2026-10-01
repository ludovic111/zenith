#!/bin/bash
# Installs zenith on this Mac:
#   1. builds the zenith server and its web app (code/),
#   2. builds zenith.app (Tauri, crates/zenith-app) with its icon,
#   3. keeps the server running in the background from login (a LaunchAgent on 127.0.0.1:4747),
#   4. opens the app.
# Run it again after changing or pulling the code to update everything.
# ZENITH_BUNDLE_ID overrides the LaunchAgent's label (default dev.zenith.app), ZENITH_CODE_HOME the
# server's state folder (default ~/.zenith/code), ZENITH_SERVER the server: rust (crates/zenith-code)
# (the default) or node (the TypeScript one in code/apps/server).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

# French when the Mac speaks French.
case "$(defaults read -g AppleLocale 2>/dev/null || echo "${LANG:-en}")" in fr*) FR=1 ;; *) FR= ;; esac
t() { if [ -n "$FR" ]; then printf "%s" "$1"; else printf "%s" "$2"; fi; }

LABEL="${ZENITH_BUNDLE_ID:-dev.zenith.app}"
PORT=4747
HOME_DIR="${ZENITH_CODE_HOME:-$HOME/.zenith/code}"   # the server's state: threads, settings, worktrees
NODE="$(command -v node || true)"
# The server's command line (zenith.app spells it too: server_command in
# crates/zenith-app/src/server.rs).
SERVER_KIND="${ZENITH_SERVER:-rust}"
case "$SERVER_KIND" in
  rust) SERVER=("$ROOT/target/release/zenith-code") ;;
  node) SERVER=("$NODE" "$ROOT/code/apps/server/dist/bin.mjs") ;;
  *) echo "ZENITH_SERVER: rust or node" >&2; exit 1 ;;
esac
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOGS="$HOME/Library/Logs/Zenith"
APP_NAME="zenith.app"
if [ -w /Applications ]; then DEST=/Applications; else DEST="$HOME/Applications"; fi

say() { printf "\n\033[1;33m✦ %s\033[0m\n" "$1"; }

# Something else on the port would keep the server from starting.
if lsof -nP -iTCP:$PORT -sTCP:LISTEN -t >/dev/null 2>&1 && ! launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
  echo "$(t "Le port $PORT est déjà pris. Arrête ce qui l'occupe puis relance ce script." "Port $PORT is taken. Stop whatever holds it and run this script again.")" >&2
  exit 1
fi

# Node 22.16+ (or 24) builds the interface (and runs the TypeScript server, with ZENITH_SERVER=node).
if [ -z "$NODE" ]; then
  echo "$(t "zenith demande Node 22.16+ (ou 24) : installe-le depuis https://nodejs.org puis relance ce script." "zenith needs Node 22.16+ (or 24): install it from https://nodejs.org and run this script again.")" >&2
  exit 1
fi
NODE_MAJOR="$(node -p 'process.versions.node.split(".")[0]')"
NODE_MINOR="$(node -p 'process.versions.node.split(".")[1]')"
if [ "$NODE_MAJOR" -lt 22 ] || { [ "$NODE_MAJOR" -eq 22 ] && [ "$NODE_MINOR" -lt 16 ]; }; then
  echo "$(t "zenith demande Node 22.16+ (ou 24) ; tu as $(node -v)." "zenith needs Node 22.16+ (or 24); you have $(node -v).")" >&2
  exit 1
fi

# zenith.app is Rust (Tauri).
if ! command -v cargo >/dev/null 2>&1; then
  echo "$(t "zenith demande Rust : installe-le depuis https://rustup.rs puis relance ce script." "zenith needs Rust: install it from https://rustup.rs and run this script again.")" >&2
  exit 1
fi

say "$(t "1/4 Construction du serveur et de l'interface" "1/4 Building the server and the interface")"
[ -d node_modules ] || npm ci
npm run code:build
[ "$SERVER_KIND" = rust ] && cargo build --release -p zenith-code

say "$(t "2/4 Compilation de zenith.app" "2/4 Compiling zenith.app")"
# What macOS shows about zenith.app, in its language (Tauri merges this Info.plist into the app's).
cat > crates/zenith-app/Info.plist <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>NSHumanReadableCopyright</key><string>$(t "Un espace de travail pour Claude Code, Codex et les autres agents de code." "A workspace for Claude Code, Codex and other coding agents.")</string>
  <key>NSAppTransportSecurity</key><dict><key>NSAllowsLocalNetworking</key><true/></dict>
</dict>
</plist>
EOF
ZENITH_AGENT_LABEL="$LABEL" ZENITH_SERVER="$SERVER_KIND" ZENITH_NODE="$NODE" ZENITH_CODE_HOME="$HOME_DIR" ZENITH_LANG="$([ -n "$FR" ] && echo fr || echo en)" \
  npx tauri build --bundles app --config "{\"identifier\":\"$LABEL.app\"}"
APP="$ROOT/target/release/bundle/macos/$APP_NAME"
codesign --force --deep --sign - "$APP" >/dev/null
{ pkill -x Zenith; pkill -x zenith; } 2>/dev/null && sleep 1 || true   # the previous version, if running
rm -rf "$DEST/$APP_NAME" "$DEST/Zénith.app"       # Zénith.app: the app's former name
ditto "$APP" "$DEST/$APP_NAME"
echo "$(t "Installée dans" "Installed in") $DEST/$APP_NAME"

say "$(t "3/4 Serveur en arrière-plan (démarre avec la session)" "3/4 Background server (starts at login)")"
mkdir -p "$LOGS" "$HOME_DIR" "$(dirname "$PLIST")"
{
  cat <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
EOF
  for arg in "${SERVER[@]}" serve --host 127.0.0.1 --port "$PORT" --base-dir "$HOME_DIR"; do
    printf '    <string>%s</string>\n' "$arg"
  done
  cat <<EOF
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
EOF
} > "$PLIST"
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
# bootout is asynchronous: wait for the old service to be gone before loading it again.
for _ in $(seq 1 20); do launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || break; sleep 0.5; done
launchctl bootstrap "gui/$(id -u)" "$PLIST"
launchctl kickstart -k "gui/$(id -u)/$LABEL"

say "$(t "4/4 Ouverture" "4/4 Opening")"
for _ in $(seq 1 30); do
  curl -fs --max-time 2 "http://127.0.0.1:$PORT/.well-known/t3/environment" >/dev/null 2>&1 && break
  sleep 1
done
open "$DEST/$APP_NAME"
echo "$(t "zenith est prêt. Clic droit sur l'icône du Dock → Options → Garder dans le Dock." "zenith is ready. Right-click its Dock icon → Options → Keep in Dock.")"
