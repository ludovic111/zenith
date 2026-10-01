// A fake `gh` for tests/github_golden.rs, installed as `<root>/bin/gh` behind a node shebang
// with __ROOT__ replaced. Both the TS oracle and the Rust side spawn it.
//
// The case is the basename of the working directory: answers come from
// `<root>/cases/<case>.json` (else `<root>/cases/default.json`), a list of
// `{when: [substring…], unless?: [substring…], stdout?, stderr?, code?}` matched in order against
// the argv joined with NULs, then the stdin. Every invocation is logged, with its stdin, to
// `<root>/log/<side>/<case>.jsonl`, where `<side>` is the content of `<root>/side`.

import * as fs from "node:fs";
import * as path from "node:path";

const root = "__ROOT__";
const args = process.argv.slice(2);
// gh reads stdin only where told to; reading it otherwise would wait on an open pipe.
const readsStdin = args.some((arg, index) => (arg === "--input" || arg === "--body-file") && args[index + 1] === "-");
const stdin = readsStdin ? fs.readFileSync(0, "utf8") : null;
const caseName = path.basename(process.cwd());
const side = fs.readFileSync(path.join(root, "side"), "utf8").trim();
const logDir = path.join(root, "log", side);
fs.mkdirSync(logDir, { recursive: true });
fs.appendFileSync(
  path.join(logDir, `${caseName}.jsonl`),
  `${JSON.stringify({ args, stdin, token: process.env.GH_TOKEN ?? null })}\n`,
);

const table = (name) => {
  const file = path.join(root, "cases", `${name}.json`);
  return fs.existsSync(file) ? JSON.parse(fs.readFileSync(file, "utf8")) : null;
};
const entries = table(caseName) ?? table("default") ?? [];
const haystack = `${args.join("\u0000")}\u0000${stdin ?? ""}`;
const entry = entries.find(
  (candidate) =>
    candidate.when.every((needle) => haystack.includes(needle)) &&
    !(candidate.unless ?? []).some((needle) => haystack.includes(needle)),
);
if (entry === undefined) {
  process.stderr.write(`unknown command: ${args.join(" ")}\n`);
  process.exit(1);
}
if (entry.stdout !== undefined) process.stdout.write(entry.stdout);
if (entry.stderr !== undefined) process.stderr.write(entry.stderr);
process.exitCode = entry.code ?? 0;
