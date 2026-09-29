import "server-only";
import { config } from "../config";
import { l10n, tr } from "../i18n";
import { cached, getJson, MissingConfig } from "../source";

/** WMO weather codes → label and symbol (day, night). */
const WMO = (): [number[], string, string, string][] => [
  [[0], tr("Grand soleil", "Clear sky"), "☀️", "🌙"],
  [[1], tr("Plutôt dégagé", "Mostly clear"), "🌤️", "🌙"],
  [[2], tr("Quelques nuages", "Partly cloudy"), "⛅", "☁️"],
  [[3], tr("Couvert", "Overcast"), "☁️", "☁️"],
  [[45, 48], tr("Brouillard", "Fog"), "🌫️", "🌫️"],
  [[51, 53, 55, 56, 57], tr("Bruine", "Drizzle"), "🌦️", "🌧️"],
  [[61, 63, 65, 66, 67, 80, 81, 82], tr("Pluie", "Rain"), "🌧️", "🌧️"],
  [[71, 73, 75, 77, 85, 86], tr("Neige", "Snow"), "🌨️", "🌨️"],
  [[95, 96, 99], tr("Orage", "Thunderstorm"), "⛈️", "⛈️"],
];

export function describe(code: number, day = true) {
  const hit = WMO().find(([codes]) => codes.includes(code));
  return { label: hit?.[1] ?? "—", icon: hit ? (day ? hit[2] : hit[3]) : "·" };
}

export type Weather = {
  now: { temp: number; feels: number; code: number; wind: number; isDay: boolean };
  days: { date: string; code: number; max: number; min: number; rain: number; sunrise: string; sunset: string }[];
};

/** The configured location, or a "to set up" error for the pages. */
export function place() {
  const loc = config().location;
  if (!loc) throw new MissingConfig(["location"]);
  return loc;
}

/** Weather at `location` (Open-Meteo, best model for the area, no key). */
export const weather = async () => {
  const loc = place();
  const tz = l10n().timeZone;
  return cached(`weather:${loc.latitude},${loc.longitude}`, 900, async (): Promise<Weather> => {
    const d = await getJson<{
      current: { temperature_2m: number; apparent_temperature: number; weather_code: number; wind_speed_10m: number; is_day: number };
      daily: { time: string[]; weather_code: number[]; temperature_2m_max: number[]; temperature_2m_min: number[]; precipitation_probability_max: number[]; sunrise: string[]; sunset: string[] };
    }>(
      `https://api.open-meteo.com/v1/forecast?latitude=${loc.latitude}&longitude=${loc.longitude}&current=temperature_2m,apparent_temperature,weather_code,wind_speed_10m,is_day&daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max,sunrise,sunset&timezone=${encodeURIComponent(tz)}&forecast_days=6`,
    );
    return {
      now: { temp: d.current.temperature_2m, feels: d.current.apparent_temperature, code: d.current.weather_code, wind: d.current.wind_speed_10m, isDay: d.current.is_day === 1 },
      days: d.daily.time.map((date, i) => ({
        date,
        code: d.daily.weather_code[i],
        max: d.daily.temperature_2m_max[i],
        min: d.daily.temperature_2m_min[i],
        rain: d.daily.precipitation_probability_max[i],
        sunrise: d.daily.sunrise[i],
        sunset: d.daily.sunset[i],
      })),
    };
  });
};
