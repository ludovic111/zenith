#!/bin/bash
# compare.sh <light|dark> <thread|home>: diff the web and GPUI captures → cmp-<which>-<scheme>.png (web | gpui | diff)
cd ~/.cache/zenith-shots
a=web-$2-$1.png; b=gpui-$2-$1.png
ae=$(compare -metric AE -fuzz 6% $a $b diff-$2-$1.png 2>&1 >/dev/null)
rmse=$(compare -metric RMSE $a $b null: 2>&1 >/dev/null)
echo "$2 $1: pixels differing (>6%): $ae of $((1440*900)); RMSE $rmse"
convert $a $b diff-$2-$1.png +append -resize 50% cmp-$2-$1.png
# Per region: sidebar (0..256), header (256.., 0..52), work below.
for r in "sidebar 256x900+0+0" "header 1184x52+256+0" "work 1184x848+256+52"; do
  set -- $r
  convert $a -crop $2 +repage /tmp/ra.png; convert $b -crop $2 +repage /tmp/rb.png
  n=$(compare -metric AE -fuzz 6% /tmp/ra.png /tmp/rb.png null: 2>&1 | cut -d' ' -f1); echo "  $1: $n px differ"
done
