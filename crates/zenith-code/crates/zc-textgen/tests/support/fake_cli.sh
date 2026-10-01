#!/bin/sh
# A recording stand-in for the `claude` and `codex` CLIs (shared by the Rust tests and the
# TypeScript oracle). It records what it was given in $FAKE_CLI_DIR/record-<n>/:
#   argv  (one NUL-terminated argument per entry)   env (`env -0`)   cwd   stdin
#   schema (a copy of the --output-schema file, when there is one)
# then answers from $FAKE_CLI_DIR: `stdout` and `stderr` are printed when present, `output` is
# written to the --output-last-message path (Codex), and `exit` holds the exit code.
dir="$FAKE_CLI_DIR"
n=1
while ! mkdir "$dir/record-$n" 2>/dev/null; do n=$((n + 1)); done
record="$dir/record-$n"
for arg in "$@"; do printf '%s\0' "$arg" >> "$record/argv"; done
: >> "$record/argv"
env -0 > "$record/env"
pwd -P > "$record/cwd"
cat > "$record/stdin"
previous=""
output_path=""
for arg in "$@"; do
  if [ "$previous" = "--output-last-message" ]; then output_path="$arg"; fi
  if [ "$previous" = "--output-schema" ]; then cp "$arg" "$record/schema"; fi
  previous="$arg"
done
if [ -n "$output_path" ] && [ -f "$dir/output" ]; then cat "$dir/output" > "$output_path"; fi
if [ -f "$dir/stderr" ]; then cat "$dir/stderr" >&2; fi
if [ -f "$dir/stdout" ]; then cat "$dir/stdout"; fi
code=0
if [ -f "$dir/exit" ]; then code=$(cat "$dir/exit"); fi
exit "$code"
