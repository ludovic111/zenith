import "server-only";
import { cached, getJson, need } from "../source";

export type OpenRouter = { credits: number; used: number; remaining: number; day: number | null; week: number | null; month: number | null };

/** OpenRouter credits: balance and spend for the day, week and month. */
export const openRouter = () =>
  cached("openrouter", 300, async (): Promise<OpenRouter> => {
    const [key] = need("OPENROUTER_API_KEY");
    const headers = { Authorization: `Bearer ${key}` };
    const [credits, info] = await Promise.all([
      getJson<{ data: { total_credits: number; total_usage: number } }>("https://openrouter.ai/api/v1/credits", { headers }),
      getJson<{ data: { usage_daily?: number; usage_weekly?: number; usage_monthly?: number } }>("https://openrouter.ai/api/v1/key", { headers }).catch(() => null),
    ]);
    const c = credits.data;
    return {
      credits: c.total_credits,
      used: c.total_usage,
      remaining: c.total_credits - c.total_usage,
      day: info?.data.usage_daily ?? null,
      week: info?.data.usage_weekly ?? null,
      month: info?.data.usage_monthly ?? null,
    };
  });
