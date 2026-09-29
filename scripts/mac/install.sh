#!/bin/bash
# Installs zenith on this Mac:
#   1. builds the dashboard (production build) and zenith code,
#   2. compiles zenith.app (a native window) with its icon,
#   3. keeps the server running in the background from login (a LaunchAgent),
#   4. opens the app.
# Run it again after changing or pulling the code to update everything.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

# French when zenith speaks French (`locale` in the config), else when the Mac does.
LOCALE="$(node -e '
  const fs = require("fs");
  const f = [process.env.ZENITH_CONFIG, "perso/zenith.config.json", "zenith.config.json"].find((x) => x && fs.existsSync(x));
  try { process.stdout.write(JSON.parse(fs.readFileSync(f, "utf8")).locale || ""); } catch {}
' 2>/dev/null)"
case "${LOCALE:-$(defaults read -g AppleLocale 2>/dev/null || echo "${LANG:-en}")}" in fr*) FR=1 ;; *) FR= ;; esac
t() { if [ -n "$FR" ]; then printf "%s" "$1"; else printf "%s" "$2"; fi; }

# The LaunchAgent label comes from `mac.bundleId` in zenith.config.json (same lookup as the server).
LABEL="$(node -e '
  const fs = require("fs");
  const f = [process.env.ZENITH_CONFIG, "perso/zenith.config.json", "zenith.config.json"].find((x) => x && fs.existsSync(x));
  let id = "dev.zenith.app";
  try { id = JSON.parse(fs.readFileSync(f, "utf8")).mac?.bundleId || id; } catch {}
  process.stdout.write(id);
')"
PORT=4747
NODE="$(command -v node)"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOGS="$HOME/Library/Logs/Zenith"
BUILD="$ROOT/.build/mac"
APP_NAME="zenith.app"
if [ -w /Applications ]; then DEST=/Applications; else DEST="$HOME/Applications"; fi

say() { printf "\n\033[1;33m✦ %s\033[0m\n" "$1"; }

# A dev server on the same port would keep the production server from starting.
if lsof -nP -iTCP:$PORT -sTCP:LISTEN -t >/dev/null 2>&1 && ! launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
  echo "$(t "Le port $PORT est déjà pris (npm run dev ?). Arrête-le puis relance ce script." "Port $PORT is taken (npm run dev?). Stop it and run this script again.")" >&2
  exit 1
fi

say "$(t "1/4 Construction du tableau de bord" "1/4 Building the dashboard")"
[ -d node_modules ] || npm ci
npm run build

# ——— zenith code (code/) ————————————————————————————————————————————————
# The coding workspace the dashboard runs on 127.0.0.1. Optional: when it can't be
# built (Node too old, no network for pnpm), the dashboard installs without it.
if [ -f code/package.json ]; then
  say "$(t "1b/4 Construction de zenith code" "1b/4 Building zenith code")"
  NODE_MAJOR="$(node -p 'process.versions.node.split(".")[0]')"
  NODE_MINOR="$(node -p 'process.versions.node.split(".")[1]')"
  if [ "$NODE_MAJOR" -lt 22 ] || { [ "$NODE_MAJOR" -eq 22 ] && [ "$NODE_MINOR" -lt 16 ]; }; then
    echo "$(t "zenith code demande Node 22.16+ (ou 24) ; tu as $(node -v). Ignoré." "zenith code needs Node 22.16+ (or 24); you have $(node -v). Skipped.")" >&2
  elif npm run code:build; then
    echo "$(t "zenith code est prêt." "zenith code is ready.")"
  else
    echo "$(t "zenith code n'a pas pu être construit : le tableau de bord s'installe sans lui. Réessaie avec « npm run code:build »." "zenith code couldn't be built: the dashboard installs without it. Retry with \"npm run code:build\".")" >&2
  fi
fi
# ——— end of zenith code ———————————————————————————————————————————————————

say "$(t "2/4 Compilation de zenith.app" "2/4 Compiling zenith.app")"
rm -rf "$BUILD"
APP="$BUILD/$APP_NAME"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
swiftc -O -o "$APP/Contents/MacOS/Zenith" scripts/mac/ZenithApp.swift -framework AppKit -framework WebKit

ICONSET="$BUILD/AppIcon.iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z $size $size scripts/mac/AppIcon.png --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  sips -z $((size * 2)) $((size * 2)) scripts/mac/AppIcon.png --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>zenith</string>
  <key>CFBundleDisplayName</key><string>zenith</string>
  <key>CFBundleIdentifier</key><string>$LABEL.app</string>
  <key>ZenithAgentLabel</key><string>$LABEL</string>
  <key>CFBundleExecutable</key><string>Zenith</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>1.0</string>
  <key>CFBundleVersion</key><string>$(date +%Y%m%d%H%M)</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSHumanReadableCopyright</key><string>$(t "Tout ce qui brille au-dessus de tes projets." "Everything that shines above your projects.")</string>
  <key>NSAppTransportSecurity</key><dict><key>NSAllowsLocalNetworking</key><true/></dict>
  <key>NSCalendarsFullAccessUsageDescription</key><string>$(t "zenith affiche tes rendez-vous dans « Ma vie » et les résume pour tes agents. Lecture seule." "zenith shows your events in My day and summarizes them for your agents. Read-only.")</string>
  <key>NSCalendarsUsageDescription</key><string>$(t "zenith affiche tes rendez-vous dans « Ma vie ». Lecture seule." "zenith shows your events in My day. Read-only.")</string>
  <key>NSRemindersFullAccessUsageDescription</key><string>$(t "zenith ajoute tes rappels à la liste « À faire ». Lecture seule." "zenith adds your reminders to the to-do list. Read-only.")</string>
  <key>NSRemindersUsageDescription</key><string>$(t "zenith ajoute tes rappels à la liste « À faire ». Lecture seule." "zenith adds your reminders to the to-do list. Read-only.")</string>
  <key>NSAppleEventsUsageDescription</key><string>$(t "zenith lit les mails non lus dans Mail et le morceau en cours dans Musique ou Spotify, seulement quand ces apps sont ouvertes. Lecture seule." "zenith reads unread mail in Mail and the song playing in Music or Spotify, only while those apps are open. Read-only.")</string>
  <key>NSContactsUsageDescription</key><string>$(t "zenith affiche les anniversaires à venir de tes contacts (nom et date seulement). Lecture seule." "zenith shows your contacts' upcoming birthdays (name and date only). Read-only.")</string>
</dict>
</plist>
EOF
codesign --force --sign - "$APP" >/dev/null
pkill -x Zenith 2>/dev/null && sleep 1 || true   # the previous version, if running
rm -rf "$DEST/$APP_NAME" "$DEST/Zénith.app"       # Zénith.app: the app's former name
ditto "$APP" "$DEST/$APP_NAME"
echo "$(t "Installée dans" "Installed in") $DEST/$APP_NAME"

say "$(t "3/4 Serveur en arrière-plan (démarre avec la session)" "3/4 Background server (starts at login)")"
mkdir -p "$LOGS" "$(dirname "$PLIST")"
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$NODE</string>
    <string>$ROOT/node_modules/next/dist/bin/next</string>
    <string>start</string>
    <string>-H</string><string>127.0.0.1</string>
    <string>-p</string><string>$PORT</string>
  </array>
  <key>WorkingDirectory</key><string>$ROOT</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>$(dirname "$NODE"):/usr/local/bin:/usr/bin:/bin</string>
    <key>NODE_ENV</key><string>production</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>$LOGS/server.log</string>
  <key>StandardErrorPath</key><string>$LOGS/server.log</string>
</dict>
</plist>
EOF
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
# bootout is asynchronous: wait for the old service to be gone before loading it again.
for _ in $(seq 1 20); do launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || break; sleep 0.5; done
launchctl bootstrap "gui/$(id -u)" "$PLIST"
launchctl kickstart -k "gui/$(id -u)/$LABEL"

say "$(t "4/4 Ouverture" "4/4 Opening")"
for _ in $(seq 1 30); do
  curl -fs "http://127.0.0.1:$PORT/api/health" >/dev/null 2>&1 && break
  sleep 1
done
open "$DEST/$APP_NAME"
echo "$(t "zenith est prêt. Clic droit sur l'icône du Dock → Options → Garder dans le Dock." "zenith is ready. Right-click its Dock icon → Options → Keep in Dock.")"
