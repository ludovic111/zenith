// The TS oracle of tests/golden.rs: runs the real `GitVcsDriver` (core and VCS-process
// drivers) of code/apps/server on the same repositories and prints wire-encoded results.
//
// Run as `node --input-type=module -e <this file>` from code/apps/server (so bare imports
// resolve against its node_modules), with ZC_SERVER_SRC pointing at code/apps/server/src and
// the request on stdin: {"baseDir": "...", "ops": [{"id", "op", "input"}]}.
// Prints one JSON object: {"<id>": {"ok": <encoded>} | {"error": <encoded error>}}.

import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Schema from "effect/Schema";
import * as NodeServices from "@effect/platform-node/NodeServices";

const src = process.env.ZC_SERVER_SRC;
const GitVcsDriver = await import(`${src}/vcs/GitVcsDriver.ts`);
const VcsProcess = await import(`${src}/vcs/VcsProcess.ts`);
const ServerConfig = await import(`${src}/config.ts`);
const Contracts = await import("@t3tools/contracts");

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));

// `baseDir/worktrees` is the worktrees directory, like the Rust side's.
const configLayer = ServerConfig.layerTest(process.cwd(), request.baseDir);

const layer = Layer.mergeAll(GitVcsDriver.vcsLayer, GitVcsDriver.layer).pipe(
  Layer.provide(configLayer),
  Layer.provideMerge(VcsProcess.layer),
  Layer.provideMerge(NodeServices.layer),
);

const encode = (schema, value) => Schema.encodeSync(Schema.toCodecJson(schema))(value);
const encodeError = (error) => {
  for (const schema of [Contracts.GitCommandError, Contracts.VcsError]) {
    try {
      return encode(schema, error);
    } catch {}
  }
  return { defect: String(error) };
};

const program = Effect.gen(function* () {
  const core = yield* GitVcsDriver.GitVcsDriver;
  const vcs = yield* GitVcsDriver.makeVcsDriverShape();
  const run = {
    status: (input) => core.status(input).pipe(Effect.map((r) => encode(Contracts.VcsStatusResult, r))),
    listRefs: (input) =>
      core.listRefs(input).pipe(Effect.map((r) => encode(Contracts.VcsListRefsResult, r))),
    reviewPreview: (input) =>
      core
        .getReviewDiffPreview(input)
        .pipe(Effect.map((r) => encode(Contracts.ReviewDiffPreviewResult, r))),
    reviewFileContents: (input) =>
      core
        .getReviewDiffFileContents(input)
        .pipe(Effect.map((r) => encode(Contracts.ReviewDiffFileContentsResult, r))),
    detectRepository: (input) =>
      vcs
        .detectRepository(input.cwd)
        .pipe(
          Effect.map((r) =>
            r === null ? null : encode(Contracts.VcsRepositoryIdentity, r),
          ),
        ),
    listWorkspaceFiles: (input) =>
      vcs
        .listWorkspaceFiles(input.cwd)
        .pipe(Effect.map((r) => encode(Contracts.VcsListWorkspaceFilesResult, r))),
    listRemotes: (input) =>
      vcs.listRemotes(input.cwd).pipe(Effect.map((r) => encode(Contracts.VcsListRemotesResult, r))),
    filterIgnoredPaths: (input) => vcs.filterIgnoredPaths(input.cwd, input.paths),
    capture: (input) => vcs.checkpoints.captureCheckpoint(input).pipe(Effect.as(null)),
    diffCheckpoints: (input) => vcs.checkpoints.diffCheckpoints(input),
    statusDetailsRemote: (input) => core.statusDetailsRemote(input.cwd, { refreshUpstream: false }),
  };
  const results = {};
  for (const { id, op, input } of request.ops) {
    const exit = yield* Effect.exit(run[op](input));
    results[id] =
      exit._tag === "Success"
        ? { ok: exit.value }
        : { error: exit.cause.reasons?.[0]?.error ? encodeError(exit.cause.reasons[0].error) : { defect: String(exit.cause) } };
  }
  return results;
});

const results = await Effect.runPromise(Effect.scoped(program).pipe(Effect.provide(layer)));
process.stdout.write(JSON.stringify(results));
