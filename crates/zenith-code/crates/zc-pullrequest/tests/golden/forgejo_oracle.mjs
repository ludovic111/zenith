// The TS oracle of tests/forgejo_golden.rs: runs the real Forgejo pull request provider of
// code/apps/server (ForgejoPullRequestProvider over ForgejoCli over the real VcsProcess) on the
// same inputs and the same fake `tea` (first on PATH) as the Rust side.
//
// Run as `node --input-type=module -e <this file>` from code/apps/server, with ZC_SERVER_SRC
// pointing at code/apps/server/src, PATH starting with the fake CLI directory, HOME at an empty
// directory (no fj keys), and the request on stdin: {"cases": [{"id", "method", "input"}]}.
// Prints one JSON object: {"<id>": {"ok": <value>} | {"error": <encoded>} | {"defect": <text>}}.
// Results are projected onto the neutral provider shapes (the keys `PullRequestProvider.ts`
// declares, and the contract's keys for a thread's comments).

import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Schema from "effect/Schema";
import * as NodeServices from "@effect/platform-node/NodeServices";
import { FetchHttpClient } from "effect/unstable/http";

const src = process.env.ZC_SERVER_SRC;
const VcsProcess = await import(`${src}/vcs/VcsProcess.ts`);
const ForgejoCli = await import(`${src}/sourceControl/ForgejoCli.ts`);
const PullRequestProvider = await import(`${src}/pullRequest/PullRequestProvider.ts`);
const ForgejoProvider = await import(`${src}/pullRequest/ForgejoPullRequestProvider.ts`);

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));

const base = Layer.mergeAll(VcsProcess.layer).pipe(Layer.provideMerge(NodeServices.layer));
const cliLayer = ForgejoCli.layer.pipe(Layer.provide(FetchHttpClient.layer));
const encodeProviderError = Schema.encodeSync(Schema.toCodecJson(PullRequestProvider.PullRequestProviderError));

const pick = (value, keys) => {
  if (value === null || value === undefined) return value;
  const out = {};
  for (const key of keys) if (value[key] !== undefined) out[key] = value[key];
  return out;
};
const CHANGE_REQUEST_KEYS = [
  "stack", "number", "title", "url", "author", "headBranch", "headRepositoryNameWithOwner", "baseBranch", "state",
  "isDraft", "mergeability", "additions", "deletions", "createdAt", "closedAt", "mergedAt", "updatedAt",
  "reviewRequestLogins", "labels", "reviewDecision", "checksState",
];
const DETAIL_KEYS = [
  ...CHANGE_REQUEST_KEYS, "body", "changedFiles", "reviewers", "checks", "mergeCapabilities", "viewerPermissions",
  "baseComparison", "behindBy", "autoMergeEnabled", "autoMergeMethod", "workflowApprovalsRequired",
];
const SUMMARY_KEYS = [
  "number", "title", "url", "headBranch", "baseBranch", "state", "isDraft", "closedAt", "mergedAt", "updatedAt",
  "author", "additions", "deletions", "changedFiles", "reviewDecision", "checksState", "mergeability",
];
const THREAD_COMMENT_KEYS = ["id", "author", "body", "createdAt", "url", "reactions"];

const shapes = {
  listChangeRequests: (page) => ({
    ...pick(page, ["truncated", "cursorAdvance", "continues"]),
    items: page.items.map((item) => pick(item, CHANGE_REQUEST_KEYS)),
  }),
  getChangeRequestSummary: (summary) => pick(summary, SUMMARY_KEYS),
  getChangeRequest: (detail) => pick(detail, DETAIL_KEYS),
  getChangeRequestActivity: (activity) => ({
    ...pick(activity, ["author", "reviewers", "comments", "commentCount", "commentsTruncated", "commits", "reactions"]),
    reviewThreads: activity.reviewThreads.map((thread) => ({
      ...thread,
      comments: thread.comments.map((comment) => pick(comment, THREAD_COMMENT_KEYS)),
    })),
  }),
  getDiff: (slice) => pick(slice, ["patch", "truncated", "nextCursor", "omittedFileStats"]),
  getFileRevisions: (value) => ({ revisions: [...value.revisions], ...pick(value, ["complete"]) }),
};

const program = Effect.gen(function* () {
  const provider = yield* ForgejoProvider.make.pipe(Effect.provide(cliLayer));
  const results = {};
  for (const testCase of request.cases) {
    const exit = yield* Effect.exit(provider[testCase.method](testCase.input));
    if (exit._tag === "Success") {
      const shape = shapes[testCase.method];
      const value = exit.value === undefined ? null : exit.value;
      results[testCase.id] = { ok: shape === undefined || value === null ? value : shape(value) };
    } else {
      const failure = exit.cause.reasons?.find((reason) => reason._tag === "Fail")?.error;
      results[testCase.id] =
        failure && failure._tag === "PullRequestProviderError"
          ? { error: encodeProviderError(failure) }
          : { defect: String(failure ?? exit.cause) };
    }
  }
  return results;
});

const results = await Effect.runPromise(program.pipe(Effect.provide(base)));
process.stdout.write(JSON.stringify(results));
