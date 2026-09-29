import "server-only";
import { config } from "../config";
import { tr } from "../i18n";
import { cached, getJson, MissingConfig } from "../source";

/*
 * Departure boards from transport.opendata.ch: Swiss public transport only (trains, trams,
 * buses, boats), no key. Outside Switzerland leave `transit` out of the config and the
 * panel stays hidden.
 */

export type Departure = { line: string; category: string; to: string; at: string; delay: number; platform: string | null };
export type Board = { station: string; departures: Departure[] };

/** Watched stop: TRANSIT_STOP, else `transit.stop` in zenith.config.json, else null. */
export const transitStop = (): string | null => process.env.TRANSIT_STOP?.trim() || config().transit?.stop?.trim() || null;

/** Next departures from a stop. */
export const departures = async (station = transitStop()) => {
  if (!station) throw new MissingConfig(["transit.stop"]);
  return cached(`transit:${station}`, 60, async (): Promise<Board> => {
    const d = await getJson<{
      station: { name: string } | null;
      stationboard: { category: string; number: string; to: string; stop: { departure: string; delay: number | null; platform: string | null; prognosis?: { platform: string | null } } }[];
    }>(`https://transport.opendata.ch/v1/stationboard?station=${encodeURIComponent(station)}&limit=12`);
    if (!d.station) throw new Error(tr(`Arrêt « ${station} » introuvable`, `Stop "${station}" not found`));
    return {
      station: d.station.name,
      departures: d.stationboard.map((s) => ({
        line: /^\d+$/.test(s.number) && ["R", "RE", "IR", "IC", "EC", "TGV", "S", "SL"].includes(s.category) ? `${s.category} ${s.number}` : s.number || s.category,
        category: s.category,
        to: s.to,
        at: s.stop.departure,
        delay: s.stop.delay ?? 0,
        platform: s.stop.prognosis?.platform ?? s.stop.platform,
      })),
    };
  });
};
