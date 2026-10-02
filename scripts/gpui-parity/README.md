# GPUI parity: the window against the web interface

The native window (`crates/zenith-app`) copies the web interface (`code/apps/web`) to the pixel.
These scripts take the reference values and screenshots from the web interface and compare them
with the window, on a Linux machine without a screen (Xvfb, software Vulkan).

Setup, once:

```bash
sudo apt-get install xvfb openbox mesa-vulkan-drivers imagemagick xdotool
mkdir -p ~/.cache/zenith-shots && cd ~/.cache/zenith-shots
npm init -y && npm i playwright && npx playwright install --with-deps chromium
# A browser session for the screenshots (revoke it afterwards with `zenith-code auth session revoke`):
node /path/to/zenith/scripts/gpui-parity/pair.mjs "$(zenith-code auth pairing create --admin --ttl 10m --json --base-dir ~/.zenith/code | jq -r .credential)"
```

Then, from `~/.cache/zenith-shots` (`ZENITH_URL` defaults to `http://127.0.0.1:4747`, `THREAD` is
the id of a thread to show; the window needs a copy of `~/.zenith/app/session.token` in
`gpui-home/app/`):

| Script | Does |
| --- | --- |
| `node vars.mjs` | every CSS variable of the web theme, light and dark, resolved by Chromium → `vars.json` |
| `python3 colors.py vars.json` | → `web-theme.json` (role → sRGB hex), copied to `crates/zenith-app/assets/web-theme.json` |
| `node material.mjs` | the sidebar's material (backdrop glows under glass 1, blurred) → `material-{light,dark}.png`, copied to `crates/zenith-app/assets/` |
| `node shots.mjs <light\|dark> <thread\|home>` | the web interface at 1440×900 → `web-<page>-<scheme>.png` |
| `node dump.mjs <light\|dark> <thread\|home>` | every visible element's box, font, colors, radius, shadow → `dump-<page>-<scheme>.json` |
| `./gpui.sh <light\|dark> <thread\|home>` | the window at 1440×900 in Xvfb → `gpui-<page>-<scheme>.png` |
| `./compare.sh <light\|dark> <thread\|home>` | pixels that differ, and `cmp-<page>-<scheme>.png` (web, window, diff side by side) |

GPUI only draws under Xvfb with a window manager running (`openbox`): without one it never gets
the visibility event that starts its frame loop.
