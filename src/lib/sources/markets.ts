import "server-only";
import { cached, getJson } from "../source";

export type Quote = { symbol: string; name: string; usd: number; change: number };

const PAIRS: [string, string, string][] = [
  ["SOLUSD", "SOL", "Solana"],
  ["XBTUSD", "BTC", "Bitcoin"],
  ["ETHUSD", "ETH", "Ether"],
];

/** Crypto prices in USD (Kraken public ticker, no key). Change since midnight UTC. */
export const crypto = () =>
  cached("markets:crypto", 120, async (): Promise<Quote[]> => {
    const d = await getJson<{ error: string[]; result: Record<string, { c: [string, string]; o: string }> }>(
      `https://api.kraken.com/0/public/Ticker?pair=${PAIRS.map(([p]) => p).join(",")}`,
    );
    if (d.error.length) throw new Error(d.error.join(", "));
    const rows = Object.entries(d.result);
    return PAIRS.map(([pair, symbol, name]) => {
      // Kraken answers with its own pair names (XXBTZUSD, XETHZUSD, SOLUSD…).
      const base = pair.slice(0, 3);
      const hit = rows.find(([k]) => k === pair || (k.includes(base) && k.endsWith("USD")));
      if (!hit) return null;
      const last = Number(hit[1].c[0]);
      const open = Number(hit[1].o);
      return { symbol, name, usd: last, change: open ? (last - open) / open : 0 };
    }).filter((q): q is Quote => !!q);
  });
