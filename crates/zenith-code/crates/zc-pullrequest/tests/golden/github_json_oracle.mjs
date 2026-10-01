// The TS oracle of tests/github_json_golden.rs: runs code/apps/server's gitHubPullRequestJson.ts
// (from source, through node and the real effect/contracts packages) on the same inputs as the
// Rust side and prints the results as plain JSON.
//
// Run as `node --input-type=module -e <this file>` from code/apps/server, with ZC_SERVER_SRC
// pointing at code/apps/server/src and the request on stdin: {"cases": [{"id", "op", ...}]}.
// Prints one JSON object: {"<id>": <result>}. A decoder's result is {"ok": <value>} or
// {"error": <formatSchemaError message>}; a Map becomes an object, a Set a sorted array.

const src = process.env.ZC_SERVER_SRC;
const Result = await import("effect/Result");
const SchemaJson = await import("@t3tools/shared/schemaJson");
const GitPatchPath = await import("@t3tools/shared/gitPatchPath");
const Json = await import(`${src}/pullRequest/gitHubPullRequestJson.ts`);

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));

function plain(value) {
  if (value instanceof Map) {
    return Object.fromEntries([...value].map(([key, entry]) => [String(key), plain(entry)]));
  }
  if (value instanceof Set) return [...value].map(plain).sort();
  if (Array.isArray(value)) return value.map(plain);
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value)
        .filter(([, entry]) => entry !== undefined)
        .map(([key, entry]) => [key, plain(entry)]),
    );
  }
  return value;
}

function decoded(result) {
  return Result.isSuccess(result)
    ? { ok: plain(result.success) === undefined ? null : plain(result.success) }
    : { error: SchemaJson.formatSchemaError(result.failure) };
}

const ops = {
  constants: () =>
    Object.fromEntries(Object.entries(Json).filter(([, value]) => typeof value !== "function")),
  pullRequestSearchGraphQlQuery: ({ rows, includeStacks }) =>
    Json.pullRequestSearchGraphQlQuery(rows, includeStacks),
  encodeGraphQlRequestJson: ({ query, variables }) =>
    Json.encodeGraphQlRequestJson({ query, variables: Object.fromEntries(variables) }),
  buildReviewSubmissionJson: (input) => Json.buildReviewSubmissionJson(input),
  buildReviewerRequestJson: ({ reviewers }) => Json.buildReviewerRequestJson(reviewers),
  buildLabelRequestJson: ({ labels }) => Json.buildLabelRequestJson(labels),
  buildPullRequestStatsGraphQlQuery: ({ changeRequests }) =>
    Json.buildPullRequestStatsGraphQlQuery(
      changeRequests.map(([repository, number]) => ({ repository, number })),
    ),
  buildPullRequestSummariesGraphQlQuery: ({ changeRequests }) =>
    Json.buildPullRequestSummariesGraphQlQuery(
      changeRequests.map(([repository, number]) => ({ repository, number })),
    ),
  buildPullRequestStackMembershipsGraphQlQuery: ({ repository, numbers }) =>
    Json.buildPullRequestStackMembershipsGraphQlQuery(repository, numbers),
  buildSetFilesViewedGraphQlMutation: ({ files }) =>
    plain(Json.buildSetFilesViewedGraphQlMutation(files.map(([path, viewed]) => ({ path, viewed })))),
  reviewThreadConversation: ({ threads }) => plain(Json.reviewThreadConversation(threads)),
  gitHubReactionContent: ({ content }) => Json.gitHubReactionContent(content),
  quoteGitPatchPath: ({ path }) => GitPatchPath.quoteGitPatchPath(path),
};

const output = {};
for (const testCase of request.cases) {
  if (testCase.op.startsWith("decode")) {
    output[testCase.id] = decoded(Json[testCase.op](testCase.raw));
  } else {
    const result = ops[testCase.op](testCase);
    output[testCase.id] = result === undefined ? null : result;
  }
}
process.stdout.write(JSON.stringify(output));
