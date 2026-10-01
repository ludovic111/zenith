import { describe, expect, it } from "vite-plus/test";

import { classifyZenithSessionsResponse } from "./sessions";

const overview = {
  generatedAt: 1,
  timeZone: "UTC",
  sessions: [],
  totals: {
    sessions: 0,
    live: 0,
    today: 0,
    costUSD: 0,
    week: { sessions: 0, costUSD: 0, tokens: 0, linesAdded: 0, linesRemoved: 0, prs: 0 },
    perDay: [],
    perProject: [],
  },
};

describe("classifyZenithSessionsResponse", () => {
  it("reads the Rust server's answer", () => {
    expect(
      classifyZenithSessionsResponse({
        status: 200,
        contentType: "application/json",
        body: overview,
      }),
    ).toEqual({ status: "ok", data: overview });
  });

  it("calls a server without the route unavailable, not broken", () => {
    expect(
      classifyZenithSessionsResponse({ status: 404, contentType: "text/plain", body: null }),
    ).toEqual({ status: "unavailable" });
    // Servers that answer unknown paths with the web app itself.
    expect(
      classifyZenithSessionsResponse({
        status: 200,
        contentType: "text/html; charset=utf-8",
        body: null,
      }),
    ).toEqual({ status: "unavailable" });
  });

  it("reports auth and server failures", () => {
    expect(
      classifyZenithSessionsResponse({ status: 401, contentType: "application/json", body: {} })
        .status,
    ).toBe("error");
    expect(
      classifyZenithSessionsResponse({ status: 500, contentType: "application/json", body: {} }),
    ).toEqual({ status: "error", message: "The server could not list sessions (500)." });
    expect(
      classifyZenithSessionsResponse({
        status: 200,
        contentType: "application/json",
        body: { sessions: "nope" },
      }).status,
    ).toBe("error");
  });
});
