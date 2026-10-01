// A fake `claude` for the process tests: it speaks stream-json from a script and records what
// it was given. FAKE_CLAUDE_SCRIPT is a JSON array of steps, run in order:
//   {"wait": "<control subtype>" | "user" | "response:<request id>", "respond"?: body, "error"?: text}
//       read stdin until a matching line (control requests are answered with `respond`/`error`)
//   {"send": message}        write one stdout line
//   {"raw": text}            write raw stdout text plus a newline
//   {"stderr": text}         write to stderr
//   {"sleep": ms}
//   {"onSigterm": "ignore" | "exit"}
//   {"exit": code}
//   {"hang": true}           stay alive (stdin EOF included) until signalled
// After the last step the fake waits for stdin EOF and exits 0, like the real CLI.
// FAKE_CLAUDE_RECORD receives one JSON line per observation: argv/env first, then stdin lines.
import fs from "node:fs";
import readline from "node:readline";

const script = JSON.parse(fs.readFileSync(process.env.FAKE_CLAUDE_SCRIPT, "utf8"));
const recordPath = process.env.FAKE_CLAUDE_RECORD;
const record = (entry) => fs.appendFileSync(recordPath, JSON.stringify(entry) + "\n");
const pick = (keys) => Object.fromEntries(keys.filter((key) => key in process.env).map((key) => [key, process.env[key]]));
record({
  argv: process.argv.slice(2),
  env: pick(["CLAUDE_CODE_ENTRYPOINT", "CLAUDE_AGENT_SDK_VERSION", "NODE_OPTIONS", "DEBUG", "FAKE_MARKER"]),
  cwd: fs.realpathSync(process.cwd()),
});

const queue = [];
let waiter = null;
let eof = false;
const wake = () => {
  if (waiter) {
    const resolve = waiter;
    waiter = null;
    resolve();
  }
};
const lines = readline.createInterface({ input: process.stdin });
lines.on("line", (line) => {
  if (!line.trim()) return;
  const message = JSON.parse(line);
  record({ stdin: message });
  queue.push(message);
  wake();
});
lines.on("close", () => {
  eof = true;
  record({ eof: true });
  wake();
});
const next = async () => {
  while (queue.length === 0) {
    if (eof) return null;
    await new Promise((resolve) => (waiter = resolve));
  }
  return queue.shift();
};
const out = (message) => process.stdout.write(JSON.stringify(message) + "\n");
const matches = (message, want) => {
  if (want === "user") return message.type === "user";
  if (want.startsWith("response:")) return message.type === "control_response" && message.response?.request_id === want.slice(9);
  return message.type === "control_request" && message.request?.subtype === want;
};

for (const step of script) {
  if (step.wait) {
    let message;
    do {
      message = await next();
      if (message === null) {
        record({ missing: step.wait });
        process.exit(97);
      }
    } while (!matches(message, step.wait));
    if (message.type === "control_request") {
      if ("respond" in step) out({ type: "control_response", response: { subtype: "success", request_id: message.request_id, response: step.respond } });
      else if (step.error) out({ type: "control_response", response: { subtype: "error", request_id: message.request_id, error: step.error } });
    }
  } else if (step.send) out(step.send);
  else if (step.raw !== undefined) process.stdout.write(step.raw + "\n");
  else if (step.stderr) process.stderr.write(step.stderr);
  else if (step.sleep) await new Promise((resolve) => setTimeout(resolve, step.sleep));
  else if (step.onSigterm === "ignore") process.on("SIGTERM", () => record({ sigterm: true }));
  else if (step.onSigterm === "exit")
    process.on("SIGTERM", () => {
      record({ sigterm: true });
      process.exit(143);
    });
  else if ("exit" in step) process.exit(step.exit);
  else if (step.hang) {
    setInterval(() => {}, 1000);
    await new Promise(() => {});
  }
}
while ((await next()) !== null) {}
process.exit(0);
