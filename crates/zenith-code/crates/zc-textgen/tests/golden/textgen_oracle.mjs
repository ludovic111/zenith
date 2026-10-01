// The TS oracle of tests/golden.rs: runs the real ClaudeTextGeneration / CodexTextGeneration
// of code/apps/server (from source) over the recording fake CLI, for a scripted list of calls.
//
// Run as `node --input-type=module -e <this file>` from code/apps/server, with ZC_SERVER_SRC
// pointing at code/apps/server/src and the request on stdin:
//   {"provider": "claude" | "codex", "config": {...settings}, "environment": {...},
//    "baseDir": "...", "models": ["slug", ...],
//    "calls": [{"op": "generateCommitMessage" | ..., "input": {...}, "fake": {stdout?, stderr?, output?, exit?},
//               "attachmentFiles": [{"name": "...", "content": "..."}]}]}
// Prints `@@ORACLE@@` then {"attachmentsDir": "...", "results": [{ok} | {error: {_tag, operation, detail}}]}.

import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Schema from "effect/Schema";
import * as NodeServices from "@effect/platform-node/NodeServices";
import * as NodeFS from "node:fs";
import * as NodePath from "node:path";

const src = process.env.ZC_SERVER_SRC;
const imp = (path) => import(`${src}/${path}`);
const Contracts = await import("@t3tools/contracts");
const ServerConfig = await imp("config.ts");
const { makeClaudeTextGeneration } = await imp("textGeneration/ClaudeTextGeneration.ts");
const { makeCodexTextGeneration } = await imp("textGeneration/CodexTextGeneration.ts");
const { SYNTHETIC_CLAUDE_MODEL_CATALOG } = await imp("provider/ClaudeModelCatalog.testFixtures.ts");

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));

const fakeDir = request.environment.FAKE_CLI_DIR;
const setFake = (fake) => {
  for (const name of ["stdout", "stderr", "output", "exit"]) {
    const file = NodePath.join(fakeDir, name);
    NodeFS.rmSync(file, { force: true });
    if (fake && fake[name] !== undefined) NodeFS.writeFileSync(file, String(fake[name]));
  }
};

const layer = ServerConfig.layerTest(request.baseDir, request.baseDir).pipe(Layer.provideMerge(NodeServices.layer));

const program = Effect.gen(function* () {
  const { attachmentsDir } = yield* ServerConfig.ServerConfig;
  const generation =
    request.provider === "claude"
      ? yield* makeClaudeTextGeneration(
          Schema.decodeUnknownSync(Contracts.ClaudeSettings)(request.config),
          request.environment,
          Effect.succeed(SYNTHETIC_CLAUDE_MODEL_CATALOG),
        )
      : yield* makeCodexTextGeneration(
          Schema.decodeUnknownSync(Contracts.CodexSettings)(request.config),
          request.environment,
          Effect.succeed(
            (request.models ?? []).map((slug) => ({ slug, name: slug, isCustom: false, capabilities: null })),
          ),
        );
  const results = [];
  for (const call of request.calls) {
    setFake(call.fake);
    for (const file of call.attachmentFiles ?? []) {
      NodeFS.mkdirSync(attachmentsDir, { recursive: true });
      NodeFS.writeFileSync(NodePath.join(attachmentsDir, file.name), file.content);
    }
    const exit = yield* Effect.exit(generation[call.op](call.input));
    if (exit._tag === "Success") {
      results.push({ ok: exit.value });
    } else {
      const failure = exit.cause.reasons?.find((reason) => reason._tag === "Fail")?.error;
      results.push({
        error: failure
          ? { _tag: failure._tag, operation: failure.operation, detail: failure.detail }
          : { defect: String(exit.cause) },
      });
    }
  }
  return { attachmentsDir, results };
});

const output = await Effect.runPromise(program.pipe(Effect.scoped, Effect.provide(layer)));
process.stdout.write(`@@ORACLE@@${JSON.stringify(output)}`);
