/**
 * The real Effect RPC client (the one apps/web uses, from `@t3tools/contracts`' WsRpcGroup) over a
 * real WebSocket, as in `apps/server/src/server.test.ts` (`wsRpcProtocolLayer`/`withWsRpcClient`).
 * Calls decode exactly like the browser does, so a passing call proves wire compatibility.
 */
import { WsRpcGroup } from "../../../packages/contracts/src/index.ts";
import { Cause, Effect, Exit, Layer, Option } from "effect";
import * as Socket from "effect/unstable/socket/Socket";
import * as RpcClient from "effect/unstable/rpc/RpcClient";
import * as RpcSerialization from "effect/unstable/rpc/RpcSerialization";
import { WebSocket } from "./ws.ts";

export const wsRpcProtocolLayer = (
  wsUrl: string,
  options: { cookie?: string; onMessage?: (message: string) => void } = {},
) => {
  const webSocketConstructorLayer = Layer.succeed(
    Socket.WebSocketConstructor,
    (socketUrl: string, socketOptions?: Socket.WebSocketConstructorOptions) => {
      // Socket.makeWebSocket only ever passes protocols here.
      const protocols =
        typeof socketOptions === "string" || Array.isArray(socketOptions)
          ? socketOptions
          : undefined;
      const socket = new WebSocket(
        socketUrl,
        protocols,
        options.cookie ? { headers: { cookie: options.cookie } } : undefined,
      );
      if (options.onMessage) {
        const onMessage = options.onMessage;
        socket.on("message", (data: unknown) => onMessage(String(data)));
      }
      return socket as unknown as globalThis.WebSocket;
    },
  );
  return RpcClient.layerProtocolSocket().pipe(
    Layer.provide(Socket.layerWebSocket(wsUrl).pipe(Layer.provide(webSocketConstructorLayer))),
    Layer.provide(RpcSerialization.layerJson),
  );
};

const makeWsRpcClient = RpcClient.make(WsRpcGroup);
export type WsRpcClient =
  typeof makeWsRpcClient extends Effect.Effect<infer Client, any, any> ? Client : never;

/**
 * Runs `f` with a connected typed client and closes it afterwards. Resolves with the success
 * value, or with `{ failure }` (the typed error, e.g. `EnvironmentAuthorizationError`, or the
 * `RpcClientError` when the socket could not open).
 */
export const withWsRpcClient = async <A, E>(
  wsUrl: string,
  f: (client: WsRpcClient) => Effect.Effect<A, E>,
  options: { cookie?: string; onMessage?: (message: string) => void } = {},
): Promise<{ ok: true; value: A } | { ok: false; failure: unknown; message: string }> => {
  const exit = await Effect.runPromiseExit(
    makeWsRpcClient.pipe(
      Effect.flatMap(f),
      Effect.provide(wsRpcProtocolLayer(wsUrl, options)),
      Effect.scoped,
    ) as Effect.Effect<A, E>,
  );
  if (Exit.isSuccess(exit)) return { ok: true, value: exit.value };
  const failure = Cause.findErrorOption(exit.cause);
  return {
    ok: false,
    failure: Option.isSome(failure) ? failure.value : Cause.squash(exit.cause),
    message: Cause.pretty(exit.cause),
  };
};
