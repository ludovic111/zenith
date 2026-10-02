/**
 * zenith's default look, the lsuite design system, as data: the `--ls-*` tokens
 * read from the bundled copy of lsuite's tokens.css (one source, no second copy
 * of the values), and the theme palette they make once index.css's default
 * mapping is applied. The palette seeds the theme editor and draws the default
 * theme's preview, so it must paint what the user sees: keep `lsuiteThemeColors`
 * in step with the default `:root` and `[data-app-sidebar]` blocks of index.css.
 */

import type { ThemeAppearance, ThemeColors } from "@t3tools/shared/themePalettes";

import tokensCss from "./lsuite-tokens.css?raw";

/** The app's id in lsuite (`<html data-app>`), which picks the signature color. */
export const LSUITE_APP = "zenith";

/** `--ls-<name>` values for zenith in one mode, keyed by `<name>`. */
export type LsuiteTokens = Readonly<Record<string, string>>;

const tokenRules = tokensCss.replace(/\/\*[\s\S]*?\*\//g, "");

/** The `--ls-*` declarations of the rule whose selector list is `selector`. */
function block(selector: string): Record<string, string> {
  for (const [, selectors = "", body = ""] of tokenRules.matchAll(/([^{}]*)\{([^{}]*)\}/g)) {
    if (selectors.replace(/\s+/g, " ").trim() !== selector) continue;
    const values: Record<string, string> = {};
    for (const [, name, value] of body.matchAll(/--ls-([a-z0-9-]+):\s*([^;]+);/g)) {
      if (name && value) values[name] = value.trim();
    }
    return values;
  }
  throw new Error(`lsuite-tokens.css has no "${selector}" rule`);
}

const tokensByMode: Partial<Record<ThemeAppearance, LsuiteTokens>> = {};

export function lsuiteTokens(mode: ThemeAppearance): LsuiteTokens {
  return (tokensByMode[mode] ??= {
    ...block(":root"),
    ...(mode === "dark" ? block(':root, [data-mode="dark"]') : block('[data-mode="light"]')),
    ...(mode === "dark"
      ? block(`[data-app="${LSUITE_APP}"], [data-app="${LSUITE_APP}"][data-mode="dark"]`)
      : block(`[data-app="${LSUITE_APP}"][data-mode="light"]`)),
  });
}

export type Rgba = Readonly<{ r: number; g: number; b: number; a: number }>;

/** The color forms tokens.css uses: hex, rgba(), and color-mix(in srgb, <color> N%, transparent). */
export function parseLsuiteColor(value: string): Rgba {
  const text = value.trim();
  const hex = /^#([0-9a-f]{6})$/i.exec(text)?.[1];
  if (hex) {
    const channel = (index: number) => Number.parseInt(hex.slice(index, index + 2), 16);
    return { r: channel(0), g: channel(2), b: channel(4), a: 1 };
  }
  const rgba = /^rgba\(([^)]+)\)$/.exec(text)?.[1];
  if (rgba) {
    const [r = 0, g = 0, b = 0, a = 1] = rgba.split(",").map(Number);
    return { r, g, b, a };
  }
  const mix = /^color-mix\(in srgb,\s*(#[0-9a-f]{6})\s+([\d.]+)%,\s*transparent\)$/i.exec(text);
  if (mix?.[1] && mix[2]) {
    return { ...parseLsuiteColor(mix[1]), a: Number(mix[2]) / 100 };
  }
  throw new Error(`Unsupported lsuite color: ${value}`);
}

/** `color` composited over an opaque `base`. */
export function over(color: Rgba, base: Rgba): Rgba {
  return mix(color, base, color.a);
}

/** color-mix(in srgb, first weight, second), both opaque. */
export function mix(first: Rgba, second: Rgba, weight: number): Rgba {
  const channel = (a: number, b: number) => a * weight + b * (1 - weight);
  return {
    r: channel(first.r, second.r),
    g: channel(first.g, second.g),
    b: channel(first.b, second.b),
    a: 1,
  };
}

/** `color` at `alpha`, as color-mix(in srgb, color alpha, transparent). */
export function withAlpha(color: Rgba, alpha: number): Rgba {
  return { ...color, a: color.a * alpha };
}

export function toHex(color: Rgba): string {
  const channel = (value: number) =>
    Math.round(Math.min(255, Math.max(0, value)))
      .toString(16)
      .padStart(2, "0");
  return `#${channel(color.r)}${channel(color.g)}${channel(color.b)}`;
}

/** The default theme's roles, flattened to opaque colors (theme palettes store opaque colors). */
export function lsuiteThemeColors(mode: ThemeAppearance): ThemeColors {
  const tokens = lsuiteTokens(mode);
  const token = (name: string) => {
    const value = tokens[name];
    if (value === undefined) throw new Error(`lsuite-tokens.css has no --ls-${name}`);
    return parseLsuiteColor(value);
  };
  const dark = mode === "dark";
  const bg = token("bg");
  const raised = token("bg-raised");
  const sunken = token("bg-sunken");
  const opaque = token("glass-opaque");
  const text = token("text");
  const text2 = token("text-2");
  const text3 = token("text-3");
  const accent = token("accent");
  const accentSoft = token("accent-soft");
  // Filled accent surfaces carry text: light mode takes step 700 (index.css).
  const primary = dark ? accent : token(`${LSUITE_APP}-700`);
  const onAccent = token("text-on-accent");
  // States keep their colors; as text in light mode they deepen toward the text color.
  const stateText = (name: string) => (dark ? token(name) : mix(token(name), text, 0.8));
  const surfaceTint = (color: Rgba, light: number, darkAlpha: number) =>
    toHex(over(withAlpha(color, dark ? darkAlpha : light), raised));
  const surface = mix(sunken, raised, 0.35);
  const hex = toHex;

  return {
    canvas: hex(raised),
    chrome: hex(bg),
    toolbar: hex(bg),
    toolbarForeground: hex(text),
    toolbarBorder: hex(over(token("line"), bg)),
    toolbarControl: hex(opaque),
    toolbarControlForeground: hex(text),
    toolbarControlHover: hex(over(accentSoft, opaque)),
    surface: hex(surface),
    surfaceRaised: hex(over(withAlpha(surface, 0.2), raised)),
    surfaceOverlay: hex(opaque),
    text: hex(text),
    textMuted: hex(text2),
    border: hex(over(token("line"), raised)),
    input: hex(over(token("line-strong"), raised)),
    focus: hex(over(token("accent-ring"), raised)),
    accent: hex(accent),
    accentForeground: hex(onAccent),
    secondary: hex(over(withAlpha(text, 0.05), raised)),
    secondaryForeground: hex(text),
    muted: hex(over(withAlpha(text, 0.05), raised)),
    mutedForeground: hex(text2),
    placeholder: hex(text2),
    secondaryLabel: hex(text2),
    iconMuted: hex(text3),
    error: hex(token("danger")),
    errorForeground: hex(stateText("danger")),
    errorSurface: surfaceTint(token("danger"), 0.08, 0.16),
    warning: hex(token("warning")),
    warningForeground: hex(stateText("warning")),
    warningSurface: surfaceTint(token("warning"), 0.08, 0.16),
    update: hex(primary),
    updateForeground: hex(token("accent-text")),
    updateSurface: surfaceTint(primary, 0.12, 0.18),
    accentSurface: hex(over(accentSoft, raised)),
    accentSurfaceForeground: hex(text),
    messageSurface: hex(over(accentSoft, raised)),
    messageForeground: hex(text),
    messageAction: hex(primary),
    messageActionForeground: hex(onAccent),
    messageActionHover: hex(mix(primary, raised, 0.9)),
    codeBackground: hex(mix(sunken, raised, 0.5)),
    codeForeground: hex(text),
    sidebar: hex(opaque),
    sidebarForeground: hex(text),
    sidebarMutedForeground: hex(text2),
    sidebarControlSurface: hex(over(withAlpha(text, 0.06), opaque)),
    sidebarRowHover: hex(over(withAlpha(text, 0.06), opaque)),
    sidebarRowActive: hex(over(accentSoft, opaque)),
    sidebarRowSelected: hex(over(withAlpha(text, 0.09), opaque)),
    sidebarBorder: hex(over(token("line"), opaque)),
    terminalBackground: hex(raised),
    terminalForeground: hex(text),
    terminalCursor: hex(accent),
    terminalSelection: hex(over(withAlpha(accent, 0.25), raised)),
    terminalScrollbar: hex(over(token("line-strong"), raised)),
    terminalScrollbarHover: hex(text3),
  };
}
