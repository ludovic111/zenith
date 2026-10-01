// The TS oracle of tests/bitbucket_golden.rs: runs the real BitbucketPullRequestProvider of
// code/apps/server (over the real BitbucketApi and its fetch HTTP client, pointed at the test's
// local stub through T3CODE_BITBUCKET_API_BASE_URL) on the same cases as the Rust side, and
// prints what each one answered.
//
// Run as `node --input-type=module -e <this file>` from code/apps/server, with ZC_SERVER_SRC
// pointing at code/apps/server/src and the request on stdin: {"cases": [{"id", "method", "input"}]}.
// After each case it waits briefly and sends `GET <base>/__case/<id>` to the stub, so the test can
// tell which requests each case made. Prints one JSON object:
// {"<id>": {"ok": <result>} | {"error": <encoded PullRequestProviderError>} | {"defect": "…"}}.

import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Schema from "effect/Schema";
import * as NodeServices from "@effect/platform-node/NodeServices";
import { FetchHttpClient } from "effect/unstable/http";

const src = process.env.ZC_SERVER_SRC;
const base = process.env.T3CODE_BITBUCKET_API_BASE_URL;
const ServerConfig = await import(`${src}/config.ts`);
const ServerSettings = await import(`${src}/serverSettings.ts`);
const GitVcsDriver = await import(`${src}/vcs/GitVcsDriver.ts`);
const VcsDriverRegistry = await import(`${src}/vcs/VcsDriverRegistry.ts`);
const VcsProcess = await import(`${src}/vcs/VcsProcess.ts`);
const BitbucketApi = await import(`${src}/sourceControl/BitbucketApi.ts`);
const BitbucketPullRequestApi = await import(`${src}/pullRequest/BitbucketPullRequestApi.ts`);
const BitbucketPullRequestProvider = await import(`${src}/pullRequest/BitbucketPullRequestProvider.ts`);
const PullRequestProvider = await import(`${src}/pullRequest/PullRequestProvider.ts`);

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));

// Bitbucket reads T3CODE_BITBUCKET_* from the environment (the Rust side gets the same values).
const bitbucketLayer = BitbucketApi.layer.pipe(
  Layer.provide(FetchHttpClient.layer),
  Layer.provide(ServerSettings.layerTest()),
  Layer.provide(
    Layer.mergeAll(
      GitVcsDriver.layer,
      VcsDriverRegistry.layer.pipe(Layer.provide(GitVcsDriver.vcsLayer)),
    ).pipe(Layer.provide(ServerConfig.layerTest(process.cwd(), { prefix: "zc-golden-config-" }))),
  ),
);
const apiLayer = BitbucketPullRequestApi.layer.pipe(Layer.provide(bitbucketLayer));

const encodeError = Schema.encodeSync(
  Schema.toCodecJson(PullRequestProvider.PullRequestProviderError),
);
// A `Map` (the file revisions) as its entries, which is how the Rust side writes it.
const plain = (value) =>
  value === undefined
    ? null
    : JSON.parse(JSON.stringify(value, (_key, inner) => (inner instanceof Map ? [...inner] : inner)));

const mark = (id) =>
  Effect.promise(async () => {
    await new Promise((resolve) => setTimeout(resolve, 50));
    await fetch(`${base}/__case/${encodeURIComponent(id)}`);
  });

const program = Effect.gen(function* () {
  // One provider for every case, so the file revision cache carries over as it does in Rust.
  const provider = yield* BitbucketPullRequestProvider.make;
  const results = {};
  for (const testCase of request.cases) {
    const exit = yield* Effect.exit(provider[testCase.method](testCase.input));
    if (exit._tag === "Success") {
      results[testCase.id] = { ok: plain(exit.value) };
    } else {
      const failure = exit.cause.reasons?.find((reason) => reason._tag === "Fail")?.error;
      results[testCase.id] =
        failure && failure._tag === "PullRequestProviderError"
          ? { error: encodeError(failure) }
          : { defect: String(failure ?? exit.cause) };
    }
    yield* mark(testCase.id);
  }
  return results;
});

const results = await Effect.runPromise(
  program.pipe(
    Effect.provide(apiLayer),
    Effect.provide(Layer.mergeAll(VcsProcess.layer).pipe(Layer.provideMerge(NodeServices.layer))),
  ),
);
process.stdout.write(JSON.stringify(results));
