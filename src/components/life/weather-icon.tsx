import { Cloud, CloudDrizzle, CloudFog, CloudLightning, CloudMoon, CloudRain, CloudSnow, CloudSun, Moon, Sun, type LucideProps } from "lucide-react";

/** The lucide symbol of a WMO weather code (Open-Meteo), day or night. */
export function WeatherIcon({ code, day = true, ...props }: { code: number; day?: boolean } & LucideProps) {
  const Icon =
    code === 0
      ? day ? Sun : Moon
      : code <= 2
        ? day ? CloudSun : CloudMoon
        : code === 3
          ? Cloud
          : code === 45 || code === 48
            ? CloudFog
            : code >= 51 && code <= 57
              ? CloudDrizzle
              : (code >= 61 && code <= 67) || (code >= 80 && code <= 82)
                ? CloudRain
                : (code >= 71 && code <= 77) || code === 85 || code === 86
                  ? CloudSnow
                  : code >= 95
                    ? CloudLightning
                    : Cloud;
  return <Icon aria-hidden {...props} />;
}
