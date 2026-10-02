//! zenith's look: the lsuite design system (`../lsuite/design/DESIGN.md`), read from its
//! tokens (`assets/lsuite-tokens.json`, a copy of `design/tokens.json`) with zenith's
//! signature blue (hue 262: accent `#72a6ff` dark, `#4777d2` light).
//!
//! Surfaces: the window is native vibrancy (macOS `NSVisualEffectView`), the chrome (sidebar,
//! title bar) is glass tier 1 over it, the work (thread log, diffs) is solid, floating
//! surfaces (menus, the palette) and dialogs use tiers 2 and 3. GPUI cannot blur what is
//! behind an element inside the window, so floating surfaces sit on the tier's opaque
//! fallback; with "Reduce transparency" on, everything does.

use gpui::{px, App, BoxShadow, Global, Hsla, Rgba, WindowAppearance};
use serde::Deserialize;

const TOKENS: &str = include_str!("../assets/lsuite-tokens.json");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

/// The lsuite tokens for one mode (all of them, used or not yet).
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Colors {
    pub bg: Hsla,
    pub bg_raised: Hsla,
    pub bg_sunken: Hsla,
    pub text: Hsla,
    pub text_2: Hsla,
    pub text_3: Hsla,
    pub text_on_accent: Hsla,
    pub line: Hsla,
    pub line_strong: Hsla,
    pub danger: Hsla,
    pub warning: Hsla,
    pub success: Hsla,
    pub glass_1: Hsla,
    pub glass_2: Hsla,
    pub glass_3: Hsla,
    pub glass_edge: Hsla,
    pub glass_highlight: Hsla,
    pub glass_opaque: Hsla,
    pub scrim: Hsla,
    pub accent: Hsla,
    /// Fills that carry text (primary buttons): the accent, one step darker in light mode,
    /// where white on step 600 stays under 4.5:1.
    pub accent_fill: Hsla,
    pub accent_hover: Hsla,
    pub accent_text: Hsla,
    pub accent_soft: Hsla,
    pub accent_ring: Hsla,
    /// A row under the pointer (text color at a low alpha).
    pub hover: Hsla,
    pub shadow: Hsla,
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub mode: Mode,
    pub colors: Colors,
    /// "Reduce transparency" is on: no vibrancy, opaque chrome.
    pub reduce_transparency: bool,
}

impl Global for Theme {}

/// The type scale (px): 11 · 12 · 13 (controls) · 15 (body) · 17 · 22 · 28.
pub mod text {
    pub const XS: f32 = 11.;
    pub const SM: f32 = 12.;
    pub const BASE: f32 = 13.;
    pub const MD: f32 = 15.;
    pub const LG: f32 = 17.;
    pub const XL: f32 = 22.;
    pub const XXL: f32 = 28.;
}

/// Radii (px): 4 · 6 (controls) · 10 (popovers) · 14 (panels) · 20 (cards).
pub mod radius {
    pub const XS: f32 = 4.;
    pub const SM: f32 = 6.;
    pub const MD: f32 = 10.;
    pub const LG: f32 = 14.;
}

#[derive(Deserialize)]
struct Tokens {
    apps: std::collections::HashMap<String, AppTokens>,
    neutral: ByMode<Neutral>,
    glass: ByMode<Glass>,
}

#[derive(Deserialize)]
struct AppTokens {
    scale: std::collections::HashMap<String, String>,
}

#[derive(Deserialize)]
struct ByMode<T> {
    dark: T,
    light: T,
}

#[derive(Deserialize)]
struct Neutral {
    bg: String,
    #[serde(rename = "bg-raised")]
    bg_raised: String,
    #[serde(rename = "bg-sunken")]
    bg_sunken: String,
    text: String,
    #[serde(rename = "text-2")]
    text_2: String,
    #[serde(rename = "text-3")]
    text_3: String,
    #[serde(rename = "text-on-accent")]
    text_on_accent: String,
    line: String,
    #[serde(rename = "line-strong")]
    line_strong: String,
    danger: String,
    warning: String,
    success: String,
}

#[derive(Deserialize)]
struct Tier {
    bg: String,
}

#[derive(Deserialize)]
struct Glass {
    #[serde(rename = "1")]
    one: Tier,
    #[serde(rename = "2")]
    two: Tier,
    #[serde(rename = "3")]
    three: Tier,
    edge: String,
    highlight: String,
    scrim: String,
    opaque: String,
}

/// `#rrggbb` or `rgba(r,g,b,a)`.
pub fn parse_color(value: &str) -> Hsla {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        let n = u32::from_str_radix(hex, 16).unwrap_or(0);
        return Rgba {
            r: ((n >> 16) & 0xff) as f32 / 255.,
            g: ((n >> 8) & 0xff) as f32 / 255.,
            b: (n & 0xff) as f32 / 255.,
            a: 1.,
        }
        .into();
    }
    if let Some(inner) = value.strip_prefix("rgba(").and_then(|v| v.strip_suffix(')')) {
        let parts: Vec<f32> = inner.split(',').filter_map(|p| p.trim().parse().ok()).collect();
        if parts.len() == 4 {
            return Rgba {
                r: parts[0] / 255.,
                g: parts[1] / 255.,
                b: parts[2] / 255.,
                a: parts[3],
            }
            .into();
        }
    }
    gpui::black()
}

fn alpha(color: Hsla, a: f32) -> Hsla {
    Hsla { a, ..color }
}

impl Theme {
    pub fn new(mode: Mode, reduce_transparency: bool) -> Self {
        let tokens: Tokens = serde_json::from_str(TOKENS).expect("assets/lsuite-tokens.json");
        let scale = &tokens.apps["zenith"].scale;
        let step = |s: &str| parse_color(&scale[s]);
        let (neutral, glass) = match mode {
            Mode::Dark => (&tokens.neutral.dark, &tokens.glass.dark),
            Mode::Light => (&tokens.neutral.light, &tokens.glass.light),
        };
        let c = parse_color;
        // [data-app="zenith"] in tokens.css.
        let (accent, accent_hover, accent_soft, accent_ring) = match mode {
            Mode::Dark => (step("400"), step("300"), alpha(step("400"), 0.18), alpha(step("300"), 0.6)),
            Mode::Light => (step("600"), step("700"), alpha(step("600"), 0.14), alpha(step("600"), 0.5)),
        };
        let accent_fill = match mode {
            Mode::Dark => step("400"),
            Mode::Light => step("700"),
        };
        let text = c(&neutral.text);
        let opaque = c(&glass.opaque);
        let tier = |bg: &str| if reduce_transparency { opaque } else { c(bg) };
        Self {
            mode,
            reduce_transparency,
            colors: Colors {
                bg: c(&neutral.bg),
                bg_raised: c(&neutral.bg_raised),
                bg_sunken: c(&neutral.bg_sunken),
                text,
                text_2: c(&neutral.text_2),
                text_3: c(&neutral.text_3),
                text_on_accent: c(&neutral.text_on_accent),
                line: c(&neutral.line),
                line_strong: c(&neutral.line_strong),
                danger: c(&neutral.danger),
                warning: c(&neutral.warning),
                success: c(&neutral.success),
                glass_1: tier(&glass.one.bg),
                glass_2: tier(&glass.two.bg),
                glass_3: tier(&glass.three.bg),
                glass_edge: c(&glass.edge),
                glass_highlight: parse_color(glass.highlight.trim_start_matches("inset 0 1px 0 ")),
                glass_opaque: opaque,
                scrim: c(&glass.scrim),
                accent,
                accent_fill,
                accent_hover,
                accent_text: accent_hover,
                accent_soft,
                accent_ring,
                hover: alpha(text, if mode == Mode::Dark { 0.06 } else { 0.05 }),
                shadow: match mode {
                    Mode::Dark => gpui::hsla(0., 0., 0., 0.45),
                    Mode::Light => gpui::hsla(220. / 360., 0.4, 0.14, 0.14),
                },
            },
        }
    }

    pub fn mode_for(appearance: Appearance, window: WindowAppearance) -> Mode {
        match appearance {
            Appearance::Dark => Mode::Dark,
            Appearance::Light => Mode::Light,
            Appearance::System => match window {
                WindowAppearance::Dark | WindowAppearance::VibrantDark => Mode::Dark,
                WindowAppearance::Light | WindowAppearance::VibrantLight => Mode::Light,
            },
        }
    }

    /// The soft drop under floating glass (`--ls-glass-shadow`).
    pub fn floating_shadow(&self) -> Vec<BoxShadow> {
        vec![
            BoxShadow {
                color: self.colors.shadow,
                offset: gpui::point(px(0.), px(12.)),
                blur_radius: px(40.),
                spread_radius: px(0.),
            },
            BoxShadow {
                color: alpha(self.colors.shadow, self.colors.shadow.a * 0.9),
                offset: gpui::point(px(0.), px(1.)),
                blur_radius: px(2.),
                spread_radius: px(0.),
            },
        ]
    }

    /// The opaque surface floating glass sits on inside the window.
    pub fn floating_bg(&self) -> Hsla {
        self.colors.glass_opaque
    }

    /// The vertical hairline between chrome and work.
    pub fn hairline(&self) -> Hsla {
        self.colors.line
    }
}

/// `cx.theme()`.
pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}

/// Whether macOS has "Reduce transparency" on.
pub fn system_reduces_transparency() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("/usr/bin/defaults")
            .args(["read", "com.apple.universalaccess", "reduceTransparency"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(c: Hsla) -> f32 {
        let rgba = c.to_rgb();
        let lin = |v: f32| if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
        0.2126 * lin(rgba.r) + 0.7152 * lin(rgba.g) + 0.0722 * lin(rgba.b)
    }

    fn over(top: Hsla, bottom: Hsla) -> Hsla {
        let (t, b) = (top.to_rgb(), bottom.to_rgb());
        let a = top.a;
        Rgba {
            r: t.r * a + b.r * (1. - a),
            g: t.g * a + b.g * (1. - a),
            b: t.b * a + b.b * (1. - a),
            a: 1.,
        }
        .into()
    }

    fn contrast(a: Hsla, b: Hsla) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    /// Text stays readable on every glass tier over the brightest and darkest backdrop, in
    /// both modes (DESIGN.md "Accessibility"): ≥ 4.5 for text, ≥ 3 for secondary text.
    #[test]
    fn glass_tiers_keep_contrast() {
        let backdrops = [parse_color("#ffffff"), parse_color("#000000")];
        for mode in [Mode::Dark, Mode::Light] {
            let theme = Theme::new(mode, false);
            let c = &theme.colors;
            for tier in [c.glass_1, c.glass_2, c.glass_3] {
                for backdrop in backdrops {
                    // Native vibrancy dims the desktop toward the mode before the tier applies.
                    let base = over(Hsla { a: 0.6, ..c.bg }, backdrop);
                    let surface = over(tier, base);
                    assert!(contrast(c.text, surface) >= 4.5, "{mode:?} text {}", contrast(c.text, surface));
                    assert!(contrast(c.text_2, surface) >= 3.0, "{mode:?} text-2 {}", contrast(c.text_2, surface));
                }
            }
            for surface in [c.bg_raised, c.bg_sunken, c.glass_opaque] {
                assert!(contrast(c.text, surface) >= 4.5);
                assert!(contrast(c.text_2, surface) >= 4.5, "{mode:?} text-2 on work");
                assert!(contrast(c.accent_text, surface) >= 3.0, "{mode:?} accent text");
            }
            assert!(contrast(c.text_on_accent, c.accent_fill) >= 4.5, "{mode:?} text on accent");
        }
    }

    #[test]
    fn colors_parse() {
        let blue = parse_color("#72a6ff").to_rgb();
        assert!((blue.r - 0x72 as f32 / 255.).abs() < 0.01);
        let edge = parse_color("rgba(255,255,255,0.09)");
        assert!((edge.a - 0.09).abs() < 0.001);
        let reduced = Theme::new(Mode::Dark, true);
        assert_eq!(reduced.colors.glass_1, reduced.colors.glass_opaque);
    }
}
