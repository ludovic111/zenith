#!/bin/bash
# Removes zenith.app and the background server. The code and the server's state are left alone.
set -uo pipefail
cd "$(dirname "$0")/../.."
# French when the Mac speaks French.
case "$(defaults read -g AppleLocale 2>/dev/null || echo "${LANG:-en}")" in fr*) FR=1 ;; *) FR= ;; esac
t() { if [ -n "$FR" ]; then printf "%s" "$1"; else printf "%s" "$2"; fi; }
LABEL="${ZENITH_BUNDLE_ID:-dev.zenith.app}"
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null
rm -f "$HOME/Library/LaunchAgents/$LABEL.plist"
rm -rf "/Applications/zenith.app" "$HOME/Applications/zenith.app"
rm -f "$HOME/.local/bin/zenith-cli" "$HOME/.local/bin/zenith-mcp" "$HOME/.lsuite/apps/zenith.json"
echo "$(t "zenith retiré (journaux conservés dans ~/Library/Logs/Zenith)." "zenith removed (logs kept in ~/Library/Logs/Zenith).")"
echo "$(t "Tes fils, réglages et worktrees restent dans ${ZENITH_CODE_HOME:-~/.zenith/code} : supprime ce dossier pour tout effacer." "Your threads, settings and worktrees stay in ${ZENITH_CODE_HOME:-~/.zenith/code}: delete that folder to erase everything.")"
