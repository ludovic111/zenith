// The TS oracle of tests/gitlab_golden.rs: runs the real GitLab pull request code of
// code/apps/server (GitLabPullRequestProvider over GitLabPullRequestCli over GitLabCli over the
// real VcsProcess, and the pure decoders of gitLabMergeRequestJson.ts) on the same inputs and the
// same fake `glab` (first on PATH) as the Rust side.
//
// Run as `node --input-type=module -e <this file>` from code/apps/server, with ZC_SERVER_SRC
// pointing at code/apps/server/src, PATH starting with the fake CLI directory, and the request on
// stdin: {"cases": [{"id", "op": "provider" | "cli" | "json", "method", "input"}]}.
// Prints one JSON object: {"<id>": {"ok": <value>} | {"error": <encoded>} | {"defect": <text>}}.
// Results are projected onto the neutral provider shapes (the keys `PullRequestProvider.ts`
// declares), so the extra fields a spread carries along do not count.

import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Result from "effect/Result";
import * as Schema from "effect/Schema";
import * as NodeServices from "@effect/platform-node/NodeServices";

const src = process.env.ZC_SERVER_SRC;
const VcsProcess = await import(`${src}/vcs/VcsProcess.ts`);
const GitLabCli = await import(`${src}/sourceControl/GitLabCli.ts`);
const PullRequestProvider = await import(`${src}/pullRequest/PullRequestProvider.ts`);
const GitLabPullRequestCli = await import(`${src}/pullRequest/GitLabPullRequestCli.ts`);
const GitLabProvider = await import(`${src}/pullRequest/GitLabPullRequestProvider.ts`);
const Json = await import(`${src}/pullRequest/gitLabMergeRequestJson.ts`);

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));

const base = Layer.mergeAll(VcsProcess.layer).pipe(Layer.provideMerge(NodeServices.layer));
const cliLayer = GitLabPullRequestCli.layer.pipe(Layer.provide(GitLabCli.layer));
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
const ACTIVITY_KEYS = ["author", "reviewers", "comments", "commentCount", "commentsTruncated", "reviewThreads", "commits", "reactions"];

const shapes = {
  listChangeRequests: (page) => ({
    ...pick(page, ["truncated", "cursorAdvance", "continues"]),
    items: page.items.map((item) => pick(item, CHANGE_REQUEST_KEYS)),
  }),
  getChangeRequest: (detail) => pick(detail, DETAIL_KEYS),
  getChangeRequestActivity: (activity) => pick(activity, ACTIVITY_KEYS),
  getDiff: (slice) => pick(slice, ["patch", "truncated", "nextCursor", "omittedFileStats"]),
  getFileRevisions: (value) => ({ revisions: [...value.revisions], ...pick(value, ["complete"]) }),
};

const listItem = (item) => pick(item, [
  "number", "title", "url", "author", "headBranch", "baseBranch", "state", "isDraft", "mergeability", "additions",
  "deletions", "createdAt", "updatedAt", "reviewRequestLogins", "labels",
]);
const detail = (value) => ({
  ...listItem(value),
  ...pick(value, [
    "body", "changedFiles", "mergedAt", "closedAt", "reviewers", "checks", "viewerCanMerge", "reviewerIds",
    "autoMergeEnabled", "autoMergeMethod", "divergedCommits",
  ]),
});

const decoders = {
  list: (raw) => Json.decodeMergeRequestListJson(raw),
  detail: (raw) => Result.map(Json.decodeMergeRequestDetailJson(raw), detail),
  viewer: (raw) => Json.decodeViewerJson(raw),
  users: (raw) => Json.decodeProjectUsersJson(raw),
  mergeCapabilities: (raw) => Json.decodeProjectMergeCapabilitiesJson(raw),
  discussions: (raw) => Json.decodeDiscussionsJson(raw),
  diffRefs: (raw) => Json.decodeDiffRefsJson(raw),
  notes: (raw) => Json.decodeNotesJson(raw),
  commits: (raw) => Json.decodeCommitsJson(raw),
  commitDiffRefs: (raw) => Json.decodeCommitDiffRefsJson(raw),
  diffs: (raw) => Json.decodeMergeRequestDiffsJson(raw),
  awards: (raw) =>
    Result.map(Json.decodeAwardEmojiJson(raw), (page) => ({ ...page, reactionsByNoteId: [...page.reactionsByNoteId] })),
  ownAward: (raw, input) => Json.decodeOwnAwardIdJson(raw, input),
  blobs: (raw) => Result.map(Json.decodeRepositoryBlobsJson(raw), (blobs) => (blobs === null ? null : [...blobs])),
};
const decodedShape = {
  list: (batch) => ({ ...batch, items: batch.items.map(listItem) }),
};

const program = Effect.gen(function* () {
  const provider = yield* GitLabProvider.make.pipe(Effect.provide(cliLayer));
  const cli = yield* GitLabPullRequestCli.GitLabPullRequestCli.pipe(Effect.provide(cliLayer));
  const results = {};
  for (const testCase of request.cases) {
    if (testCase.op === "json") {
      const decoded = decoders[testCase.method](testCase.input.raw, testCase.input.args);
      results[testCase.id] = Result.isSuccess(decoded)
        ? { ok: (decodedShape[testCase.method] ?? ((value) => value))(decoded.success) }
        : { error: "decode" };
      continue;
    }
    const run = testCase.op === "provider" ? provider[testCase.method](testCase.input) : cli[testCase.method](testCase.input);
    const exit = yield* Effect.exit(run);
    if (exit._tag === "Success") {
      const shape = testCase.op === "provider" ? shapes[testCase.method] : undefined;
      const value = exit.value === undefined ? null : exit.value;
      results[testCase.id] = { ok: shape === undefined || value === null ? value : shape(value) };
    } else {
      const failure = exit.cause.reasons?.find((reason) => reason._tag === "Fail")?.error;
      if (failure && failure._tag === "PullRequestProviderError") {
        results[testCase.id] = { error: encodeProviderError(failure) };
      } else if (failure && typeof failure._tag === "string") {
        results[testCase.id] = { error: { tag: failure._tag, message: failure.message } };
      } else {
        results[testCase.id] = { defect: String(failure ?? exit.cause) };
      }
    }
  }
  return results;
});

const results = await Effect.runPromise(program.pipe(Effect.provide(base)));
process.stdout.write(JSON.stringify(results));
