// The TS oracle of tests/golden.rs: runs the real source control code of code/apps/server
// (providers over the real VcsProcess, discovery specs, pure helpers) on the same inputs and
// the same fake CLIs (first on PATH) as the Rust side, and prints wire-encoded results.
//
// Run as `node --input-type=module -e <this file>` from code/apps/server, with ZC_SERVER_SRC
// pointing at code/apps/server/src, PATH starting with the fake CLI directory, HOME at a temp
// directory, and the request on stdin: {"cases": [{"id", "op", ...}]}.
// Prints one JSON object: {"<id>": {"ok": <encoded>} | {"error": <encoded>}}.

import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Schema from "effect/Schema";
import * as NodeServices from "@effect/platform-node/NodeServices";
import { FetchHttpClient } from "effect/unstable/http";

const src = process.env.ZC_SERVER_SRC;
const Contracts = await import("@t3tools/contracts");
const SharedSourceControl = await import("@t3tools/shared/sourceControl");
const VcsProcess = await import(`${src}/vcs/VcsProcess.ts`);
const GitHubCli = await import(`${src}/sourceControl/GitHubCli.ts`);
const GitLabCli = await import(`${src}/sourceControl/GitLabCli.ts`);
const AzureDevOpsCli = await import(`${src}/sourceControl/AzureDevOpsCli.ts`);
const ForgejoCli = await import(`${src}/sourceControl/ForgejoCli.ts`);
const GitHubProvider = await import(`${src}/sourceControl/GitHubSourceControlProvider.ts`);
const GitLabProvider = await import(`${src}/sourceControl/GitLabSourceControlProvider.ts`);
const AzureProvider = await import(`${src}/sourceControl/AzureDevOpsSourceControlProvider.ts`);
const ForgejoProvider = await import(`${src}/sourceControl/ForgejoSourceControlProvider.ts`);
const Discovery = await import(`${src}/sourceControl/SourceControlProviderDiscovery.ts`);
const ProviderModule = await import(`${src}/sourceControl/SourceControlProvider.ts`);
const RateLimit = await import(`${src}/sourceControl/SourceControlRateLimit.ts`);
const CloneProgress = await import(`${src}/project/gitCloneProgress.ts`);
const AzurePullRequests = await import(`${src}/sourceControl/azureDevOpsPullRequests.ts`);
const ServerConfig = await import(`${src}/config.ts`);
const ServerSettings = await import(`${src}/serverSettings.ts`);
const GitVcsDriver = await import(`${src}/vcs/GitVcsDriver.ts`);
const VcsDriverRegistry = await import(`${src}/vcs/VcsDriverRegistry.ts`);
const BitbucketApi = await import(`${src}/sourceControl/BitbucketApi.ts`);
const BitbucketProvider = await import(`${src}/sourceControl/BitbucketSourceControlProvider.ts`);

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));

const base = Layer.mergeAll(VcsProcess.layer).pipe(Layer.provideMerge(NodeServices.layer));
const encode = (schema, value) => Schema.encodeSync(Schema.toCodecJson(schema))(value);

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

const providers = {
  bitbucket: BitbucketProvider.make.pipe(Effect.provide(bitbucketLayer)),
  github: GitHubProvider.make.pipe(Effect.provide(GitHubCli.layer)),
  gitlab: GitLabProvider.make.pipe(Effect.provide(GitLabCli.layer)),
  "azure-devops": AzureProvider.make.pipe(Effect.provide(AzureDevOpsCli.layer)),
  forgejo: ForgejoProvider.make.pipe(
    Effect.provide(ForgejoCli.layer.pipe(Layer.provide(FetchHttpClient.layer))),
  ),
};

const specs = {
  github: GitHubProvider.discovery,
  gitlab: GitLabProvider.discovery,
  "azure-devops": AzureProvider.discovery,
  forgejo: ForgejoProvider.discovery,
};

const resultSchemas = {
  getChangeRequest: Contracts.ChangeRequest,
  listChangeRequests: Schema.Array(Contracts.ChangeRequest),
  getRepositoryCloneUrls: Contracts.SourceControlRepositoryCloneUrls,
  createRepository: Contracts.SourceControlRepositoryCloneUrls,
  getDefaultBranch: Schema.NullOr(Schema.String),
  createChangeRequest: Schema.Void,
  checkoutChangeRequest: Schema.Void,
};

const pure = {
  transportSafe: ([value]) => ProviderModule.transportSafeSourceControlErrorValue(value),
  cloneProgress: ([line]) => CloneProgress.parseGitCloneProgressLine(line),
  retryAt: ([value, now]) => RateLimit.retryAtFromHeader(value ?? undefined, now) ?? null,
  detectProvider: ([url]) => SharedSourceControl.detectSourceControlProviderFromRemoteUrl(url) ?? null,
  forgejoRemote: ([url]) => ForgejoCli.parseForgejoRemote(url),
  forgejoLogin: ([logins, remote, requestedHost, hostOnly]) =>
    ForgejoCli.matchForgejoLogin(
      ForgejoCli.parseForgejoLogins(JSON.stringify(logins)),
      ForgejoCli.parseForgejoRemote(remote),
      requestedHost ?? undefined,
      hostOnly ?? false,
    )?.name ?? null,
  azureWebUrl: ([input]) => AzurePullRequests.azureDevOpsPullRequestWebUrl(input),
  ownerRef: ([selector]) => ProviderModule.parseSourceControlOwnerRef(selector) ?? null,
};

const program = Effect.gen(function* () {
  const vcsProcess = yield* VcsProcess.VcsProcess;
  const results = {};
  for (const testCase of request.cases) {
    const run = Effect.gen(function* () {
      switch (testCase.op) {
        case "provider": {
          const provider = yield* providers[testCase.kind];
          const value = yield* provider[testCase.method](testCase.input);
          return encode(resultSchemas[testCase.method], value);
        }
        case "parseAuth":
          return encode(Contracts.SourceControlProviderAuth, specs[testCase.kind].parseAuth(testCase.input));
        case "refine": {
          const refined = specs[testCase.kind].refineUnknownRemote?.(testCase.input) ?? null;
          return refined === null ? null : encode(Contracts.SourceControlProviderInfo, refined);
        }
        case "probeBitbucket": {
          const spec = yield* BitbucketProvider.makeDiscovery.pipe(Effect.provide(bitbucketLayer));
          const item = yield* Discovery.probeSourceControlProvider({ spec, process: vcsProcess, cwd: testCase.cwd });
          return encode(Contracts.SourceControlProviderDiscoveryItem, item);
        }
        case "probe": {
          const item = yield* Discovery.probeSourceControlProvider({
            spec: specs[testCase.kind],
            process: vcsProcess,
            cwd: testCase.cwd,
          });
          return encode(Contracts.SourceControlProviderDiscoveryItem, item);
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
      if (failure && failure._tag === "SourceControlProviderError") {
        results[testCase.id] = { error: encode(Contracts.SourceControlProviderError, failure) };
      } else {
        results[testCase.id] = { defect: String(failure ?? exit.cause) };
      }
    }
  }
  return results;
});

const results = await Effect.runPromise(program.pipe(Effect.provide(base)));
process.stdout.write(JSON.stringify(results));
