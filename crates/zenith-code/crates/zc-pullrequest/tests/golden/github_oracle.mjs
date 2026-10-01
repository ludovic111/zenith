// The TS oracle of tests/github_golden.rs: runs the real GitHubPullRequestProvider of
// code/apps/server (over GitHubPullRequestCli, GitHubCli, the GraphQL budget, the rate limits and
// the real VcsProcess) on the same inputs and against the same fake `gh` (first on PATH) as the
// Rust side, and prints every result projected onto the neutral provider interfaces.
//
// Run as `node --input-type=module -e <this file>` from code/apps/server, with ZC_SERVER_SRC
// pointing at code/apps/server/src, PATH starting with the fake `gh` directory, and the request
// on stdin: {"cases": [{"id", "method", "input"}]}. Each case gets a fresh provider (its own
// caches, budget and rate limits), as the Rust side does.
// Prints one JSON object: {"<id>": {"ok": <projected>} | {"error": <encoded>} | {"defect": "…"}}.

import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Schema from "effect/Schema";
import * as NodeServices from "@effect/platform-node/NodeServices";

const src = process.env.ZC_SERVER_SRC;
const VcsProcess = await import(`${src}/vcs/VcsProcess.ts`);
const GitHubCli = await import(`${src}/sourceControl/GitHubCli.ts`);
const GitHubPullRequestCli = await import(`${src}/pullRequest/GitHubPullRequestCli.ts`);
const GitHubProvider = await import(`${src}/pullRequest/GitHubPullRequestProvider.ts`);
const ProviderModule = await import(`${src}/pullRequest/PullRequestProvider.ts`);

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));

const pick = (value, keys) => {
  if (value === null || value === undefined) return value;
  const picked = {};
  for (const key of keys) if (value[key] !== undefined) picked[key] = value[key];
  return picked;
};

const CHANGE_REQUEST = [
  "stack",
  "number",
  "title",
  "url",
  "author",
  "headBranch",
  "headRepositoryNameWithOwner",
  "baseBranch",
  "state",
  "isDraft",
  "mergeability",
  "additions",
  "deletions",
  "createdAt",
  "closedAt",
  "mergedAt",
  "updatedAt",
  "reviewRequestLogins",
  "labels",
  "reviewDecision",
  "checksState",
];
const DETAIL = [
  ...CHANGE_REQUEST,
  "body",
  "changedFiles",
  "reviewers",
  "checks",
  "mergeCapabilities",
  "viewerPermissions",
  "baseComparison",
  "behindBy",
  "autoMergeEnabled",
  "autoMergeMethod",
  "workflowApprovalsRequired",
];
const SUMMARY = [
  "number",
  "title",
  "url",
  "headBranch",
  "baseBranch",
  "state",
  "isDraft",
  "closedAt",
  "mergedAt",
  "updatedAt",
  "author",
  "additions",
  "deletions",
  "changedFiles",
  "reviewDecision",
  "checksState",
  "mergeability",
];
const STACK_LAYER = ["title", "isDraft", "headSha", "number", "headBranch", "state"];
const ACTIVITY = [
  "author",
  "reviewers",
  "comments",
  "commentCount",
  "commentsTruncated",
  "reviewThreads",
  "commits",
  "reactions",
];

/** The neutral interface each method answers with, as the Rust side serializes it. */
const project = {
  getViewer: (value) => value,
  getRoutingIdentity: (value) => pick(value, ["accountId", "viewer"]),
  listChangeRequests: (value) => ({
    ...pick(value, ["truncated", "cursorAdvance", "continues"]),
    items: value.items.map((item) => pick(item, CHANGE_REQUEST)),
  }),
  listChangeRequestsAcross: (value) => ({
    truncated: value.truncated,
    items: value.items.map((item) => pick(item, ["repository", ...CHANGE_REQUEST])),
  }),
  listChangeRequestStats: (value) => value.map((stat) => pick(stat, ["repository", "number", "additions", "deletions"])),
  getChangeRequest: (value) => pick(value, DETAIL),
  getChangeRequestPreview: (value) =>
    pick(value, ["number", "title", "url", "author", "state", "isDraft", "createdAt"]),
  getChangeRequestSummary: (value) => pick(value, SUMMARY),
  getChangeRequestStack: (value) =>
    value === null
      ? null
      : {
          ...pick(value, ["id", "number", "url", "base"]),
          layers: value.layers.map((layer) => pick(layer, STACK_LAYER)),
        },
  getChangeRequestActivity: (value) => pick(value, ACTIVITY),
  getReviewThreadComments: (value) => pick(value, ["comments", "nextCursor"]),
  getViewerPermissions: (value) => value,
  getDiff: (value) => pick(value, ["patch", "truncated", "nextCursor", "omittedFileStats"]),
  getDiffFileContents: (value) => pick(value, ["oldContents", "newContents"]),
  getFilesViewed: (value) => pick(value, ["files", "truncated"]),
  listReviewerCandidates: (value) => value,
  listLabelCandidates: (value) => value,
};

const encodeError = Schema.encodeSync(Schema.toCodecJson(ProviderModule.PullRequestProviderError));

const program = Effect.gen(function* () {
  const results = {};
  for (const testCase of request.cases) {
    const layer = GitHubPullRequestCli.layer.pipe(Layer.provideMerge(GitHubCli.layer));
    const run = Effect.gen(function* () {
      const provider = yield* GitHubProvider.make;
      const value = yield* provider[testCase.method](testCase.input);
      return value === undefined ? null : (project[testCase.method] ?? (() => null))(value);
    }).pipe(Effect.provide(layer));
    const exit = yield* Effect.exit(run);
    if (exit._tag === "Success") {
      results[testCase.id] = { ok: JSON.parse(JSON.stringify(exit.value)) };
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

const results = await Effect.runPromise(
  program.pipe(Effect.provide(VcsProcess.layer.pipe(Layer.provideMerge(NodeServices.layer)))),
);
process.stdout.write(JSON.stringify(results));
