#!/usr/bin/env node
/**
 * Compares two recordings request by request, after normalization (lib/normalize.ts).
 *
 *   node scripts/compat/diff.ts left.jsonl right.jsonl [--sequences ordinal|rebase|exact]
 *     [--ignore-keys <regex>] [--ignore-paths <regex>] [--keep-volatile] [--json]
 *
 * Exit code 1 when anything differs. Typical uses: a capture against TS vs its replay against
 * Rust; two replays against TS (to see what is nondeterministic in the session itself).
 */
import * as NodeUtil from "node:util";
import { diffRecordings, formatDiffReport } from "./lib/diff.ts";
import { readRecording } from "./lib/recording.ts";

const { values, positionals } = NodeUtil.parseArgs({
  allowPositionals: true,
  options: {
    sequences: { type: "string", default: "ordinal" },
    "ignore-keys": { type: "string" },
    "ignore-paths": { type: "string" },
    "keep-volatile": { type: "boolean", default: false },
    json: { type: "boolean", default: false },
    max: { type: "string", default: "12" },
  },
});
if (positionals.length !== 2) {
  process.stderr.write("usage: node scripts/compat/diff.ts <left.jsonl> <right.jsonl> [options]\n");
  process.exit(2);
}
const diffs = diffRecordings(readRecording(positionals[0]!), readRecording(positionals[1]!), {
  sequences: values.sequences as "ordinal" | "rebase" | "exact",
  keepVolatile: values["keep-volatile"],
  ...(values["ignore-keys"] ? { ignoreKeys: new RegExp(values["ignore-keys"]) } : {}),
  ...(values["ignore-paths"] ? { ignorePaths: new RegExp(values["ignore-paths"]) } : {}),
});
process.stdout.write(
  values.json
    ? `${JSON.stringify(diffs, null, 2)}\n`
    : `${formatDiffReport(diffs, Number(values.max))}\n`,
);
process.exit(diffs.some((d) => d.status !== "same") ? 1 : 0);
