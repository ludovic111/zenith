#!/bin/bash
# Removes zenith.app and the background server. The code, perso/ and .env.local are left alone.
set -uo pipefail
cd "$(dirname "$0")/../.."
case "$(defaults read -g AppleLocale 2>/dev/null || echo "${LANG:-en}")" in fr*) FR=1 ;; *) FR= ;; esac
t() { if [ -n "$FR" ]; then printf "%s" "$1"; else printf "%s" "$2"; fi; }
LABEL="$(node -e '
  const fs = require("fs");
  const f = [process.env.ZENITH_CONFIG, "perso/zenith.config.json", "zenith.config.json"].find((x) => x && fs.existsSync(x));
  let id = "dev.zenith.app";
  try { id = JSON.parse(fs.readFileSync(f, "utf8")).mac?.bundleId || id; } catch {}
  process.stdout.write(id);
')"
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null
rm -f "$HOME/Library/LaunchAgents/$LABEL.plist"
rm -rf "/Applications/zenith.app" "$HOME/Applications/zenith.app"
echo "$(t "zenith retiré (journaux conservés dans ~/Library/Logs/Zenith)." "zenith removed (logs kept in ~/Library/Logs/Zenith).")"

# zenith code: its server stops with the dashboard; threads and settings stay in its state folder.
echo "$(t "Les fils de zenith code restent dans ~/.zenith/code (ou code.home) : supprime ce dossier pour tout effacer." "zenith code threads stay in ~/.zenith/code (or code.home): delete that folder to erase everything.")"
