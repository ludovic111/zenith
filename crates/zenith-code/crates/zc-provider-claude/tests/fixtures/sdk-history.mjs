// History check generator: run the Agent SDK's own `getSessionMessages` and `forkSession` over a
// COPY of a Claude config dir and record what they return (and the transcript a fork writes).
//   CLAUDE_CONFIG_DIR=<copy> node sdk-history.mjs <sdk package dir> <out.json>
// The copy is modified (forks are written into it); never point this at a real ~/.claude.
import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

const [sdkDir, outPath] = process.argv.slice(2);
const configDir = process.env.CLAUDE_CONFIG_DIR;
if (!configDir || configDir === path.join(process.env.HOME ?? "", ".claude")) throw new Error("CLAUDE_CONFIG_DIR must point at a copy");
const { getSessionMessages, forkSession } = await import(pathToFileURL(path.join(sdkDir, "sdk.mjs")).href);

const sessions = [];
for (const dir of fs.readdirSync(path.join(configDir, "projects"))) {
  for (const name of fs.readdirSync(path.join(configDir, "projects", dir))) {
    if (name.endsWith(".jsonl")) sessions.push({ dir, sessionId: name.slice(0, -6) });
  }
}
const results = [];
for (const { dir, sessionId } of sessions.sort((a, b) => a.sessionId.localeCompare(b.sessionId))) {
  const messages = await getSessionMessages(sessionId);
  const withSystem = await getSessionMessages(sessionId, { includeSystemMessages: true });
  // Fork at the user message halfway through, like a rollback would.
  const users = messages.filter((m) => m.type === "user");
  const upTo = users.length > 1 ? users[Math.floor(users.length / 2)].uuid : undefined;
  let fork = null;
  if (upTo) {
    const { sessionId: forkId } = await forkSession(sessionId, { upToMessageId: upTo });
    const forkFile = path.join(configDir, "projects", dir, `${forkId}.jsonl`);
    fork = {
      upTo,
      sessionId: forkId,
      messages: await getSessionMessages(forkId, { includeSystemMessages: true }),
      lines: fs.readFileSync(forkFile, "utf8").split("\n").filter(Boolean).map((line) => JSON.parse(line)),
    };
  }
  results.push({ dir, sessionId, messages, withSystem, fork });
  console.log(sessionId, messages.length, withSystem.length, fork ? fork.lines.length : "-");
}
fs.writeFileSync(outPath, JSON.stringify(results));
