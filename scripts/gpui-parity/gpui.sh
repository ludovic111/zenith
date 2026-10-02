#!/bin/bash
# gpui.sh <light|dark> <thread|home> (ZENITH_BIN: the window, default this checkout's debug build): screenshot the GPUI window at 1440x900 → gpui-<which>-<scheme>.png
set -e
cd ~/.cache/zenith-shots
scheme=${1:-light}; which=${2:-thread}; bin=${ZENITH_BIN:-$(dirname "$(readlink -f "$0")")/../../target/debug/zenith}
export DISPLAY=:99 VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json ZENITH_APP_HOME=$PWD/gpui-home/app ZENITH_NO_UPDATE=1
pgrep -x Xvfb >/dev/null || { Xvfb :99 -screen 0 1600x1000x24 -nolisten tcp >/tmp/xvfb.log 2>&1 & sleep 2; }
pgrep -x openbox >/dev/null || { openbox >/tmp/openbox.log 2>&1 & sleep 1; }
pkill -x zenith || true; sleep 0.5
thread=null; [ "$which" = thread ] && thread="\"${THREAD:?set THREAD to a thread id}\""
echo "{\"appearance\":\"$scheme\",\"sidebarVisible\":true,\"sidebarWidth\":256,\"lastThread\":$thread,\"checkForUpdates\":false}" > gpui-home/app/window.json
extra=(); [ "$which" = home ] && extra=(--page new)
"$bin" "${extra[@]}" >/tmp/zenith-app.log 2>&1 &
for i in $(seq 1 40); do W=$(xdotool search --name '^zenith$' 2>/dev/null | head -1); [ -n "$W" ] && break; sleep 0.5; done
xdotool windowsize "$W" 1440 900; xdotool windowmove "$W" 0 0
sleep ${WAIT:-12}
import -window "$W" gpui-$which-$scheme.png
pkill -x zenith || true
convert gpui-$which-$scheme.png -format "%wx%h\n" info:
