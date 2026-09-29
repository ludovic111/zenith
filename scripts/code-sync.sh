#!/bin/bash
# Brings upstream T3 Code changes into code/ (zenith code).
#
#   bash scripts/code-sync.sh [branch-or-commit]     (default: main)
#
# Diffs upstream from the commit recorded in code/UPSTREAM to the new head, drops
# the folders zenith code does not ship (mobile, desktop, marketing, infra, .repos)
# and the lockfile, applies the rest with a 3-way merge, then reinstalls so the
# lockfile is regenerated. Conflicts are left in place (and listed) for you to fix.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UPSTREAM_URL="${T3CODE_UPSTREAM_URL:-https://github.com/pingdotgg/t3code.git}"
TARGET="${1:-main}"
EXCLUDES=(apps/mobile apps/marketing apps/desktop infra .repos pnpm-lock.yaml)

cd "$ROOT"
FROM="$(tr -d '[:space:]' < code/UPSTREAM)"
[ -n "$FROM" ] || { echo "code/UPSTREAM is empty." >&2; exit 1; }

if ! git diff --quiet -- code || ! git diff --cached --quiet -- code; then
  echo "code/ has uncommitted changes: commit or stash them first." >&2
  exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Fetching $UPSTREAM_URL…"
git clone --quiet --filter=blob:none --no-checkout "$UPSTREAM_URL" "$TMP/t3code"
UP="$TMP/t3code"
if git -C "$UP" rev-parse --verify --quiet "origin/$TARGET^{commit}" >/dev/null; then
  TO="$(git -C "$UP" rev-parse "origin/$TARGET")"
else
  TO="$(git -C "$UP" rev-parse "$TARGET^{commit}")"
fi
git -C "$UP" cat-file -e "$FROM^{commit}" 2>/dev/null || { echo "Upstream has no commit $FROM." >&2; exit 1; }

if [ "$FROM" = "$TO" ]; then
  echo "Already on upstream ${TO:0:10}."
  exit 0
fi

PATHSPEC=(.)
for path in "${EXCLUDES[@]}"; do PATHSPEC+=(":(exclude)$path"); done
git -C "$UP" diff --binary --full-index "$FROM" "$TO" -- "${PATHSPEC[@]}" > "$TMP/upstream.patch"
COMMITS="$(git -C "$UP" rev-list --count "$FROM..$TO")"
echo "Upstream ${FROM:0:10} → ${TO:0:10}: $COMMITS commits, $(grep -c '^diff --git' "$TMP/upstream.patch" || true) files."

if [ ! -s "$TMP/upstream.patch" ]; then
  echo "$TO" > code/UPSTREAM
  echo "Nothing to apply outside the excluded folders. code/UPSTREAM updated."
  exit 0
fi

STATUS=0
git apply -3 --directory=code "$TMP/upstream.patch" || STATUS=$?
echo "$TO" > code/UPSTREAM

CONFLICTS="$(git diff --name-only --diff-filter=U -- code || true)"
if [ -n "$CONFLICTS" ]; then
  echo
  echo "Conflicts to resolve (look for <<<<<<< markers):"
  echo "$CONFLICTS" | sed 's/^/  /'
  echo
  echo "Then: npm run code:build, and commit with the upstream range in the message."
  exit 1
elif [ "$STATUS" -ne 0 ]; then
  echo "git apply failed without conflict markers (see above). code/UPSTREAM was updated; revert it with git checkout code/UPSTREAM." >&2
  exit "$STATUS"
fi

echo "Applied cleanly. Reinstalling to regenerate the lockfile…"
npm run --silent code:install
echo
echo "Done. Next: npm run code:build, check zenith code, then commit:"
echo "  git add code && git commit -m \"zenith code: sync upstream T3 Code ${FROM:0:7}..${TO:0:7}\""
