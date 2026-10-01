// The TS oracle of tests/golden.rs (repository identity part): resolves each cwd with the real
// RepositoryIdentityResolver of code/apps/server (plain layer, no refinement).
//
// Run as `node --input-type=module -e <this file>` from code/apps/server, with ZC_SERVER_SRC
// pointing at code/apps/server/src and the request on stdin: {"cwds": ["...", ...]}.
// Prints `@@ORACLE@@` then [identity | null, ...].

import * as Effect from "effect/Effect";
import * as NodeServices from "@effect/platform-node/NodeServices";

const src = process.env.ZC_SERVER_SRC;
const RepositoryIdentityResolver = await import(`${src}/project/RepositoryIdentityResolver.ts`);

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));

const results = await Effect.runPromise(
  Effect.gen(function* () {
    const resolver = yield* RepositoryIdentityResolver.RepositoryIdentityResolver;
    const out = [];
    for (const cwd of request.cwds) out.push(yield* resolver.resolve(cwd));
    return out;
  }).pipe(Effect.provide(RepositoryIdentityResolver.layer), Effect.provide(NodeServices.layer)),
);
process.stdout.write("@@ORACLE@@" + JSON.stringify(results));
