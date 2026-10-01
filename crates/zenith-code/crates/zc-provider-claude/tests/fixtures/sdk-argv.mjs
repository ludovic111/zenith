// Gate 3 generator: run the Agent SDK's own `query()` on each case and capture what it would
// spawn (command, argv, cwd, env) and the first stdin line (`initialize`), through
// `spawnClaudeCodeProcess` and a fake process. Nothing is spawned and no model is called.
//   node sdk-argv.mjs <sdk package dir> <cases.json> <out.json>
import fs from "node:fs";
import path from "node:path";
import { PassThrough } from "node:stream";
import { EventEmitter } from "node:events";
import { pathToFileURL } from "node:url";

const [sdkDir, casesPath, outPath] = process.argv.slice(2);
const { query } = await import(pathToFileURL(path.join(sdkDir, "sdk.mjs")).href);
const cases = JSON.parse(fs.readFileSync(casesPath, "utf8"));

function fakeProcess(onLine) {
  const process = new EventEmitter();
  const stdin = new PassThrough();
  let buffer = "";
  stdin.on("data", (chunk) => {
    buffer += chunk.toString("utf8");
    let index;
    while ((index = buffer.indexOf("\n")) >= 0) {
      onLine(buffer.slice(0, index));
      buffer = buffer.slice(index + 1);
    }
  });
  Object.assign(process, { stdin, stdout: new PassThrough(), stderr: new PassThrough(), killed: false, exitCode: null, pid: 4242 });
  process.kill = () => {
    process.killed = true;
    return true;
  };
  return process;
}

const results = [];
for (const testCase of cases) {
  const options = { ...testCase.sdkOptions };
  if (options.canUseTool) options.canUseTool = async () => ({ behavior: "deny", message: "test" });
  if (options.onUserDialog) options.onUserDialog = async () => ({ behavior: "cancelled" });
  const captured = await new Promise((resolve, reject) => {
    let spawned;
    const timer = setTimeout(() => reject(new Error(`${testCase.name}: no initialize within 5s`)), 5000);
    options.spawnClaudeCodeProcess = (spec) => {
      spawned = { command: spec.command, args: spec.args, cwd: spec.cwd ?? null, env: spec.env };
      return fakeProcess((line) => {
        const message = JSON.parse(line);
        if (message.type === "control_request" && message.request?.subtype === "initialize") {
          clearTimeout(timer);
          resolve({ ...spawned, initialize: message.request });
        }
      });
    };
    const prompt = (async function* () {
      await new Promise(() => {});
    })();
    const q = query({ prompt, options });
    q.next().catch(() => {});
    q.initializationResult?.().catch(() => {});
  });
  const env = Object.fromEntries(Object.entries(captured.env).filter(([, value]) => value !== undefined));
  results.push({ name: testCase.name, sdkOptions: testCase.sdkOptions, command: captured.command, args: captured.args, cwd: captured.cwd, env, initialize: captured.initialize });
}
fs.writeFileSync(outPath, JSON.stringify(results, null, 2) + "\n");
console.log(`wrote ${results.length} cases to ${outPath}`);
process.exit(0);
