import { NextResponse, type NextRequest } from "next/server";
import { tr } from "@/lib/i18n";

/**
 * zenith holds your life: it only answers this computer. A domain name pointing at
 * 127.0.0.1 (DNS rebinding) is refused too, even from a local browser.
 */
const LOCAL = /^(127\.0\.0\.1|localhost|\[::1\])(:\d+)?$/;

export function proxy(request: NextRequest) {
  if (!LOCAL.test(request.headers.get("host") ?? "")) return new NextResponse(tr("zenith ne répond qu'en local.", "zenith only answers local requests."), { status: 403 });
  return NextResponse.next();
}
