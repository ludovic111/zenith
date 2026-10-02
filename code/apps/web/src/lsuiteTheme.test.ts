import { describe, expect, it } from "vite-plus/test";

import indexHtml from "../index.html?raw";
import { DEFAULT_CODE_FONT_STACK, DEFAULT_SANS_FONT_STACK } from "./appearanceFonts";
import {
  lsuiteThemeColors,
  lsuiteTokens,
  mix,
  over,
  parseLsuiteColor,
  type Rgba,
  withAlpha,
} from "./lsuiteTheme";

const MODES = ["dark", "light"] as const;

function luminance({ r, g, b }: Rgba): number {
  const linear = (value: number) => {
    const channel = value / 255;
    return channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
}

function contrast(first: Rgba, second: Rgba): number {
  const [light, dark] = [luminance(first), luminance(second)].toSorted((a, b) => b - a);
  return (light! + 0.05) / (dark! + 0.05);
}

function families(stack: string): string[] {
  return stack.split(",").map((family) => family.trim().replace(/^(['"])(.*)\1$/, "$2"));
}

describe("the lsuite tokens", () => {
  it("give zenith its blue in both modes", () => {
    expect(lsuiteTokens("dark").accent).toBe("#72a6ff");
    expect(lsuiteTokens("light").accent).toBe("#4777d2");
  });

  it("give both modes the same tokens", () => {
    expect(Object.keys(lsuiteTokens("light")).toSorted()).toEqual(
      Object.keys(lsuiteTokens("dark")).toSorted(),
    );
  });

  it("are what the boot splash paints before the stylesheet loads", () => {
    const constant = (name: string) =>
      indexHtml.match(new RegExp(`${name} = "(#[0-9a-f]{6})"`))?.[1];
    expect(constant("LIGHT_BACKGROUND")).toBe(lsuiteTokens("light").bg);
    expect(constant("DARK_BACKGROUND")).toBe(lsuiteTokens("dark").bg);
    expect(indexHtml).toContain('<html lang="en" data-app="zenith">');
  });

  it("set the font fallbacks a custom font is prepended to", () => {
    expect(families(DEFAULT_SANS_FONT_STACK)).toEqual(families(lsuiteTokens("dark")["font-sans"]!));
    expect(families(DEFAULT_CODE_FONT_STACK)[0]).toBe("IBM Plex Mono");
  });
});

describe.each(MODES)("zenith's default theme, %s", (mode) => {
  const tokens = lsuiteTokens(mode);
  const token = (name: string) => parseLsuiteColor(tokens[name]!);
  const colors = lsuiteThemeColors(mode);
  const role = (name: keyof typeof colors) => parseLsuiteColor(colors[name]);
  const text = token("text");
  const text2 = token("text-2");
  const text3 = token("text-3");
  const states = [role("errorForeground"), role("warningForeground")];
  const success = mode === "dark" ? token("success") : mix(token("success"), text, 0.8); // index.css

  // What the glass blurs: the window backdrop at its plainest and at the peak
  // of each glow, and the solid work surfaces a floating tier can cover.
  const bg = token("bg");
  const strength = Number(tokens["aurora-strength"]);
  const backdrops = {
    backdrop: bg,
    "first glow": over(withAlpha(token("aurora-a"), strength), bg),
    "second glow": over(withAlpha(token("aurora-b"), strength), bg),
    "work surface": token("bg-raised"),
    "sunken surface": token("bg-sunken"),
  };
  const byLuminance = Object.values(backdrops).toSorted((a, b) => luminance(a) - luminance(b));
  const extremes = { darkest: byLuminance[0]!, brightest: byLuminance.at(-1)! };

  it.each([1, 2, 3])(
    "keeps text legible on glass %i over the darkest and brightest backdrop",
    (tier) => {
      for (const [name, backdrop] of Object.entries(extremes)) {
        for (const glass of [token(`glass-${tier}-bg`), token("glass-opaque")]) {
          const surface = over(glass, backdrop);
          expect(contrast(text, surface), `${name} text`).toBeGreaterThanOrEqual(7);
          expect(contrast(text2, surface), `${name} text-2`).toBeGreaterThanOrEqual(4.5);
          expect(contrast(text3, surface), `${name} text-3 (icons)`).toBeGreaterThanOrEqual(3);
          expect(contrast(token("accent-text"), surface), `${name} accent`).toBeGreaterThanOrEqual(
            4.5,
          );
          for (const state of [...states, success]) {
            expect(contrast(state, surface), `${name} state`).toBeGreaterThanOrEqual(4.5);
          }
          // An active row (accent-soft) on the tier still reads.
          const active = over(token("accent-soft"), surface);
          expect(contrast(text, active), `${name} active row`).toBeGreaterThanOrEqual(7);
          expect(contrast(text2, active), `${name} active row text-2`).toBeGreaterThanOrEqual(4.5);
        }
      }
    },
  );

  it("keeps text legible on the solid work surfaces", () => {
    for (const surface of [
      role("canvas"),
      role("surface"),
      role("codeBackground"),
      role("messageSurface"),
      role("accentSurface"),
    ]) {
      expect(contrast(text, surface)).toBeGreaterThanOrEqual(7);
      expect(contrast(text2, surface)).toBeGreaterThanOrEqual(4.5);
      expect(contrast(text3, surface)).toBeGreaterThanOrEqual(3);
      expect(contrast(role("updateForeground"), surface)).toBeGreaterThanOrEqual(4.5);
      for (const state of [...states, success]) {
        expect(contrast(state, surface)).toBeGreaterThanOrEqual(4.5);
      }
    }
  });

  it("keeps the primary action readable and visible", () => {
    // A filled accent surface carrying text (light mode takes step 700 for it).
    expect(contrast(role("messageActionForeground"), role("messageAction"))).toBeGreaterThanOrEqual(
      4.5,
    );
    expect(contrast(role("messageAction"), role("canvas"))).toBeGreaterThanOrEqual(4.5);
    expect(contrast(role("accent"), role("canvas"))).toBeGreaterThanOrEqual(3);
  });
});
