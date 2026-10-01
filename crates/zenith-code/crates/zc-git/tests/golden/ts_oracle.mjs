// The TS oracle of tests/golden.rs: runs the real GitManager of code/apps/server (from source)
// over the real git driver, VcsProcess and GitHub provider (the fake `gh` is first on PATH), with
// a fake text generation, on the repository the request names, and prints what it saw.
//
// Run as `node --input-type=module -e <this file>` from code/apps/server, with ZC_SERVER_SRC
// pointing at code/apps/server/src and the request on stdin:
// {"cwd", "action", "actionId", "settings", "commit": {"subject", "body"}, "pr": {"title", "body"}}.
// Prints {"events": [...], "result" | "error", "textInputs": {"commit": [...], "pr": [...]}}.

import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Schema from "effect/Schema";
import * as NodeServices from "@effect/platform-node/NodeServices";

const src = process.env.ZC_SERVER_SRC;
const Contracts = await import("@t3tools/contracts");
const VcsProcess = await import(`${src}/vcs/VcsProcess.ts`);
const GitVcsDriver = await import(`${src}/vcs/GitVcsDriver.ts`);
const GitHubCli = await import(`${src}/sourceControl/GitHubCli.ts`);
const GitHubProvider = await import(`${src}/sourceControl/GitHubSourceControlProvider.ts`);
const Registry = await import(`${src}/sourceControl/SourceControlProviderRegistry.ts`);
const TextGeneration = await import(`${src}/textGeneration/TextGeneration.ts`);
const ProviderRegistry = await import(`${src}/provider/Services/ProviderRegistry.ts`);
const SetupRunner = await import(`${src}/project/ProjectSetupScriptRunner.ts`);
const ServerSettings = await import(`${src}/serverSettings.ts`);
const ServerConfig = await import(`${src}/config.ts`);
const GitManager = await import(`${src}/git/GitManager.ts`);

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));

const encode = (schema, value) => Schema.encodeSync(Schema.toCodecJson(schema))(value);
const textInputs = { commit: [], pr: [] };

const textGeneration = {
  generateCommitMessage: (input) =>
    Effect.sync(() => {
      textInputs.commit.push(input);
      return {
        subject: request.commit.subject,
        body: request.commit.body,
        ...(input.includeBranch ? { branch: request.commit.branch } : {}),
      };
    }),
  generatePrContent: (input) =>
    Effect.sync(() => {
      textInputs.pr.push(input);
      return { title: request.pr.title, body: request.pr.body };
    }),
  generateBranchName: () => Effect.succeed({ branch: "unused" }),
  generateThreadTitle: () => Effect.succeed({ title: "unused" }),
};

const serverConfigLayer = ServerConfig.layerTest(process.cwd(), { prefix: "zc-git-golden-" });
const vcsDriverLayer = GitVcsDriver.layer.pipe(
  Layer.provideMerge(VcsProcess.layer),
  Layer.provideMerge(NodeServices.layer),
  Layer.provideMerge(serverConfigLayer),
);
const registryLayer = Layer.effect(
  Registry.SourceControlProviderRegistry,
  GitHubProvider.make.pipe(
    Effect.map((provider) =>
      Registry.SourceControlProviderRegistry.of({
        resolveLink: (input) => provider.resolveLink?.(input),
        get: () => Effect.succeed(provider),
        resolveHandle: () => Effect.succeed({ provider, context: null }),
        resolve: () => Effect.succeed(provider),
        discover: Effect.succeed([]),
      }),
    ),
    Effect.provide(GitHubCli.layer.pipe(Layer.provide(VcsProcess.layer))),
  ),
);
const managerLayer = Layer.mergeAll(
  Layer.succeed(TextGeneration.TextGeneration, textGeneration),
  Layer.mock(ProviderRegistry.ProviderRegistry)({ getProviders: Effect.succeed([]) }),
  Layer.succeed(SetupRunner.ProjectSetupScriptRunner, {
    runForThread: () => Effect.succeed({ status: "no-script" }),
  }),
  vcsDriverLayer,
  ServerSettings.layerTest(request.settings ?? {}),
).pipe(Layer.provideMerge(registryLayer), Layer.provideMerge(NodeServices.layer));

const events = [];
const program = Effect.gen(function* () {
  const manager = yield* GitManager.make;
  return yield* Effect.exit(
    manager.runStackedAction(
      {
        actionId: request.actionId,
        cwd: request.cwd,
        action: request.action,
        ...(request.commitMessage ? { commitMessage: request.commitMessage } : {}),
        ...(request.featureBranch ? { featureBranch: true } : {}),
      },
      {
        actionId: request.actionId,
        progressReporter: {
          publish: (event) => Effect.sync(() => events.push(encode(Contracts.GitActionProgressEvent, event))),
        },
      },
    ),
  );
}).pipe(Effect.provide(managerLayer), Effect.scoped);

const exit = await Effect.runPromise(program);
const output = { events, textInputs };
if (exit._tag === "Success") {
  output.result = encode(Contracts.GitRunStackedActionResult, exit.value);
} else {
  const failure = exit.cause.reasons?.find((reason) => reason._tag === "Fail");
  output.error = failure ? { tag: failure.error._tag, message: failure.error.message } : { defect: String(exit.cause) };
}
process.stdout.write(JSON.stringify(output));
