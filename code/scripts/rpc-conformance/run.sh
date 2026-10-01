#!/bin/sh
# Builds the Rust server, starts `zenith-code dev-serve --conformance` on a free port and
# runs conformance.mjs (the real Effect RPC client) against it, ping/pong check included.
#
#   code/scripts/rpc-conformance/run.sh            # full run (~45 s)
#   PING_SECONDS=0 code/scripts/rpc-conformance/run.sh   # skip the 32 s ping/pong wait
#
# Needs node and an installed code/node_modules (pnpm install in code/, or a symlink to
# another checkout's). effect is linked into ./node_modules (git-ignored) on first run.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../.." && pwd)

if [ ! -e "$here/node_modules/effect" ]; then
  effect=$(ls -d "$repo"/code/node_modules/.pnpm/effect@4.0.0-rc.115*/node_modules/effect 2>/dev/null | head -n 1 || true)
  if [ -z "$effect" ]; then
    echo "effect@4.0.0-rc.115 not found under code/node_modules: run pnpm install in code/" >&2
    exit 2
  fi
  mkdir -p "$here/node_modules"
  ln -s "$effect" "$here/node_modules/effect"
fi

cargo build --quiet -p zenith-code --manifest-path "$repo/Cargo.toml"
target=$(cargo metadata --format-version 1 --no-deps --manifest-path "$repo/Cargo.toml" |
  node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).target_directory))')

log=$(mktemp)
"$target/debug/zenith-code" dev-serve --conformance --port 0 >"$log" 2>&1 &
pid=$!
trap 'kill "$pid" 2>/dev/null || true; rm -f "$log"' EXIT INT TERM

addr=""
i=0
while [ $i -lt 100 ]; do
  addr=$(sed -n 's|^listening on http://||p' "$log" | head -n 1)
  [ -n "$addr" ] && break
  if ! kill -0 "$pid" 2>/dev/null; then
    cat "$log" >&2
    exit 1
  fi
  sleep 0.1
  i=$((i + 1))
done
[ -n "$addr" ] || { echo "the server did not start" >&2; cat "$log" >&2; exit 1; }

node "$here/conformance.mjs" "ws://$addr/ws"
