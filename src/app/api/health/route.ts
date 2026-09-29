/** Answers as soon as the server is ready: the Mac app uses it to know when to show the dashboard. */
export function GET() {
  return Response.json({ ok: true });
}
