import { createFileRoute } from "@tanstack/react-router";

import { SessionsPage } from "../zenith/SessionsPage";

export const Route = createFileRoute("/sessions")({
  component: SessionsPage,
});
