// The TS oracle of tests/azure_golden.rs: runs the real Azure DevOps pull request code of
// code/apps/server (the provider over AzureDevOpsPullRequestCli over AzureDevOpsCli over the real
// VcsProcess, the JSON decoders, the diff synthesis and jsdiff itself) on the same inputs and the
// same fake `az` (first on PATH) as the Rust side, and prints plain JSON results.
//
// Run as `node --input-type=module -e <this file>` from code/apps/server, with ZC_SERVER_SRC
// pointing at code/apps/server/src, PATH starting with the fake `az` directory, and the request on
// stdin: {"cases": [{"id", "op", ...}]}.
// Prints one JSON object: {"<id>": {"ok": <value>} | {"error": <encoded>} | {"defect": <text>}}.

import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Result from "effect/Result";
import * as Schema from "effect/Schema";
import * as NodeServices from "@effect/platform-node/NodeServices";
import { structuredPatch } from "diff";

const src = process.env.ZC_SERVER_SRC;
const VcsProcess = await import(`${src}/vcs/VcsProcess.ts`);
const AzureDevOpsCli = await import(`${src}/sourceControl/AzureDevOpsCli.ts`);
const PullRequestCli = await import(`${src}/pullRequest/AzureDevOpsPullRequestCli.ts`);
const Provider = await import(`${src}/pullRequest/AzureDevOpsPullRequestProvider.ts`);
const ProviderModule = await import(`${src}/pullRequest/PullRequestProvider.ts`);
const Json = await import(`${src}/pullRequest/azureDevOpsPullRequestJson.ts`);
const Diff = await import(`${src}/pullRequest/azureDevOpsDiff.ts`);

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));

const base = Layer.mergeAll(VcsProcess.layer).pipe(Layer.provideMerge(NodeServices.layer));
const cliLayer = PullRequestCli.layer.pipe(Layer.provide(AzureDevOpsCli.layer));
const encodeError = Schema.encodeSync(Schema.toCodecJson(ProviderModule.PullRequestProviderError));

/** Maps (file revisions) as their entries; everything else as JSON sees it. */
const plain = (value) =>
  JSON.parse(JSON.stringify(value ?? null, (_, inner) => (inner instanceof Map ? [...inner.entries()] : inner)));

const decoders = {
  pullRequestList: Json.decodePullRequestListJson,
  pullRequest: Json.decodePullRequestJson,
  viewer: Json.decodeViewerJson,
  threads: Json.decodeThreadsJson,
  iterations: Json.decodeIterationsJson,
  iterationChanges: Json.decodeIterationChangesJson,
  itemContent: Json.decodeItemContentJson,
};

const pure = {
  decode: ([decoder, raw]) => {
    const result = decoders[decoder](raw);
    return Result.isSuccess(result) ? { ok: plain(result.success) } : { failed: true };
  },
  filePatch: ([change, texts]) => Diff.azureDevOpsFilePatch({ change, texts }),
  unreadableFilePatch: ([change]) => Diff.azureDevOpsUnreadableFilePatch(change),
  structuredPatch: ([oldText, newText, maxEditLength]) => {
    const patch = structuredPatch("a", "b", oldText, newText, undefined, undefined, {
      context: 3,
      ...(maxEditLength === null ? {} : { maxEditLength }),
    });
    return patch === undefined ? null : patch.hunks;
  },
  parseCursor: ([raw]) => Diff.parseAzureDevOpsDiffCursor(raw),
  localeCompare: ([left, right]) => Math.sign(left.localeCompare(right)),
};

const program = Effect.gen(function* () {
  const results = {};
  for (const testCase of request.cases) {
    const run = Effect.gen(function* () {
      switch (testCase.op) {
        case "provider": {
          // A fresh provider per case, like the Rust side, so the location cache of one case
          // cannot answer for the next.
          const provider = yield* Provider.make.pipe(Effect.provide(cliLayer));
          return plain(yield* provider[testCase.method](testCase.input));
        }
        case "pure":
          return pure[testCase.fn](testCase.args);
        default:
          throw new Error(`unknown op ${testCase.op}`);
      }
    });
    const exit = yield* Effect.exit(run);
    if (exit._tag === "Success") {
      results[testCase.id] = { ok: exit.value === undefined ? null : exit.value };
    } else {
      const failure = exit.cause.reasons?.find((reason) => reason._tag === "Fail")?.error;
      if (failure && failure._tag === "PullRequestProviderError") {
        results[testCase.id] = { error: encodeError(failure) };
      } else {
        results[testCase.id] = { defect: String(failure ?? exit.cause) };
      }
    }
  }
  return results;
});

const results = await Effect.runPromise(program.pipe(Effect.provide(base)));
process.stdout.write(JSON.stringify(results));
