import Link from "next/link";
import { tr } from "@/lib/i18n";

export type Planet = { id: string; name: string; href: string; glow: string; color: string; emoji: string; up: boolean | null };

/** Solar system: the zenith sun in the middle, one project per orbit. Hover pauses it. */
export function Orbit({ planets }: { planets: Planet[] }) {
  // Orbit diameters in % of the box: 34, 48, 62, 76, 90 for up to five projects, tighter beyond.
  const step = planets.length > 5 ? 56 / (planets.length - 1) : 14;
  const radius = (i: number) => 34 + i * step;
  return (
    <div className="relative mx-auto aspect-square w-full max-w-[520px]">
      {/* Sun */}
      <div className="absolute inset-0 m-auto size-[22%]">
        <div className="absolute inset-[-40%] rounded-full" style={{ background: "radial-gradient(circle, #FFD16688 0%, #FF8A4C33 40%, transparent 70%)", animation: "sun-breathe 6s ease-in-out infinite" }} />
        <div className="absolute inset-0 grid place-items-center rounded-full shadow-[0_0_80px_#FFD16699]" style={{ background: "radial-gradient(circle at 40% 35%, #FFF6D8 0%, #FFD166 35%, #FF8A4C 75%, #E5418A 100%)" }}>
          <span className="font-display text-[clamp(18px,4vw,34px)] font-black text-[#3a1d05]">Z</span>
        </div>
      </div>
      {planets.map((p, i) => {
        const size = `${radius(i)}%`;
        const seconds = 38 + i * 14;
        const period = `${seconds}s`;
        // Spreads the planets around the circle: 0°, 144°, 288°, 72°, 216°…
        const phase = `-${(((i * 2) % 5) / 5) * seconds}s`;
        return (
          <div key={p.id} className="orbit" style={{ width: size, height: size, ["--period" as string]: period, ["--phase" as string]: phase }}>
            <Link
              href={p.href}
              className="group absolute left-1/2 top-0 -translate-x-1/2 -translate-y-1/2 outline-none"
              aria-label={`${p.name} — ${p.up == null ? tr("état inconnu", "status unknown") : p.up ? tr("en ligne", "online") : tr("hors ligne", "offline")}`}
            >
              <div className="counter flex flex-col items-center">
                <span
                  className="relative grid size-9 place-items-center rounded-full text-base transition-transform duration-300 group-hover:scale-125 group-focus-visible:scale-125 sm:size-11 sm:text-lg"
                  style={{
                    background: `radial-gradient(circle at 35% 30%, #ffffff55, ${p.glow} 45%, ${p.color} 100%)`,
                    boxShadow: `0 0 24px ${p.glow}aa, 0 0 2px #fff inset`,
                  }}
                >
                  {p.emoji}
                  <span
                    className="absolute -right-0.5 -top-0.5 size-3 rounded-full ring-2 ring-[#07060d]"
                    style={{ background: p.up == null ? "var(--ink-3)" : p.up ? "var(--good)" : "var(--bad)" }}
                  />
                </span>
                <span className="mt-1.5 whitespace-nowrap rounded-full bg-black/50 px-2 py-0.5 text-[11px] font-medium text-ink backdrop-blur transition group-hover:bg-black/80">
                  {p.name}
                </span>
              </div>
            </Link>
          </div>
        );
      })}
    </div>
  );
}
