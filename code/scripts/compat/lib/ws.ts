/**
 * The `ws` package, reached through `@effect/platform-node` (the workspace has no direct `ws`
 * dependency, and scripts must not add one).
 */
import { NodeWS } from "@effect/platform-node/NodeSocket";

export const WebSocket = NodeWS.WebSocket;
export const WebSocketServer = NodeWS.WebSocketServer;
export type WebSocket = InstanceType<typeof NodeWS.WebSocket>;
export type RawData = NodeWS.RawData;

export const rawDataToString = (data: RawData): string =>
  Array.isArray(data)
    ? Buffer.concat(data).toString("utf8")
    : Buffer.isBuffer(data)
      ? data.toString("utf8")
      : Buffer.from(data).toString("utf8");
