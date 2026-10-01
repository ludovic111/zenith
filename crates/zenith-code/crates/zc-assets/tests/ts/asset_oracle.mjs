// The TS oracle of tests/interop.rs: the real AssetAccess and AttachmentUpload of
// code/apps/server, over a base directory the Rust side shares (same signing key file, same
// attachments directory), so a URL minted by either side is resolved by the other.
//
// Run as `node --input-type=module -e <this file>` from code/apps/server, with ZC_SERVER_SRC
// pointing at code/apps/server/src and the request on stdin:
//   {"baseDir": "...", "cases": [{"id", "op": "issue" | "resolve" | "issueUpload" | "validateUpload", ...}]}
// Prints one JSON object: {"<id>": {"ok": <value>} | {"error": <encoded error>}}.

import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Schema from "effect/Schema";
import * as NodeServices from "@effect/platform-node/NodeServices";

const src = process.env.ZC_SERVER_SRC;
const Contracts = await import("@t3tools/contracts");
const ServerConfig = await import(`${src}/config.ts`);
const ServerSecretStore = await import(`${src}/auth/ServerSecretStore.ts`);
const WorkspacePaths = await import(`${src}/workspace/WorkspacePaths.ts`);
const ProjectFaviconResolver = await import(`${src}/project/ProjectFaviconResolver.ts`);
const T3ProjectFileLoader = await import(`${src}/project/T3ProjectFileLoader.ts`);
const NativeAppIconResolver = await import(`${src}/assets/NativeAppIconResolver.ts`);
const AssetAccess = await import(`${src}/assets/AssetAccess.ts`);
const AttachmentUpload = await import(`${src}/assets/AttachmentUpload.ts`);

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));

const configLayer = ServerConfig.layerTest(process.cwd(), request.baseDir);
const layer = Layer.mergeAll(
  configLayer,
  WorkspacePaths.layer,
  ProjectFaviconResolver.layer.pipe(
    Layer.provide(WorkspacePaths.layer),
    Layer.provide(T3ProjectFileLoader.layer),
  ),
  NativeAppIconResolver.layer.pipe(Layer.provide(configLayer)),
  ServerSecretStore.layer.pipe(Layer.provide(configLayer)),
).pipe(Layer.provideMerge(NodeServices.layer));

const encodeAssetError = Schema.encodeSync(Schema.toCodecJson(Contracts.AssetAccessError));
const decodeResource = Schema.decodeUnknownSync(Contracts.AssetResource);
const decodeUploadInput = Schema.decodeUnknownSync(Contracts.AttachmentCreateUploadUrlInput);

const run = (testCase) => {
  switch (testCase.op) {
    case "issue":
      return AssetAccess.issueAssetUrl({
        resource: decodeResource(testCase.resource),
        ...(testCase.workspaceRoot !== undefined ? { workspaceRoot: testCase.workspaceRoot } : {}),
        ...(testCase.projectFaviconPath !== undefined
          ? { projectFaviconPath: testCase.projectFaviconPath }
          : {}),
      }).pipe(Effect.mapError(encodeAssetError));
    case "resolve":
      return AssetAccess.resolveAsset(testCase.token, testCase.name).pipe(
        Effect.map((asset) => {
          if (asset === null) return null;
          const { file, ...rest } = asset;
          return { ...rest, ...(file !== undefined ? { opened: true } : {}) };
        }),
      );
    case "issueUpload":
      return AttachmentUpload.issueAttachmentUploadUrl(decodeUploadInput(testCase.input));
    case "validateUpload":
      return AttachmentUpload.validateAttachmentUploadToken(testCase.token);
    default:
      return Effect.die(new Error(`unknown op ${testCase.op}`));
  }
};

const results = {};
for (const testCase of request.cases) {
  results[testCase.id] = await Effect.runPromise(
    run(testCase).pipe(
      Effect.map((ok) => ({ ok })),
      Effect.catch((error) => Effect.succeed({ error })),
      Effect.scoped,
      Effect.provide(layer),
    ),
  );
}
process.stdout.write(JSON.stringify(results));
