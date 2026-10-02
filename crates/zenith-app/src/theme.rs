//! zenith's look: the web interface's (`code/apps/web`, its default zenith theme), copied
//! to the pixel. Its colors are resolved by Chromium and kept in `assets/web-theme.json`
//! (role → light and dark, see `scripts/gpui-parity`); the window uses them by the web's own
//! role names.
//!
//! Surfaces, as on the web: the sidebar is glass 1 over the window's backdrop (two soft glows),
//! blurred; GPUI cannot blur, so that material is drawn by Chromium once and carried as an
//! image (`assets/material-{light,dark}.png`, stretched to the window). The top bar is glass 1
//! over the work, which is solid; floating surfaces (menus, the palette, the composer) use
//! glass 2 and dialogs glass 3, on their opaque fallback when "Reduce transparency" is on.

use gpui::{px, App, BoxShadow, Global, Hsla, Rgba, WindowAppearance};
use serde::Deserialize;

const WEB_THEME: &str = include_str!("../assets/web-theme.json");

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

/// The web theme's colors for one mode. The first names are the window's own (kept from
/// before), each documented with the web role it now holds.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Colors {
    /// `--ls-bg`: the window's backdrop.
    pub bg: Hsla,
    /// `--background`: the work (thread log, pages).
    pub bg_raised: Hsla,
    /// `--card`.
    pub bg_sunken: Hsla,
    /// `--foreground`.
    pub text: Hsla,
    /// `--muted-foreground`.
    pub text_2: Hsla,
    /// `--ls-text-3` (`--icon-muted`).
    pub text_3: Hsla,
    /// `--primary-foreground`.
    pub text_on_accent: Hsla,
    /// `--border`.
    pub line: Hsla,
    /// `--input`.
    pub line_strong: Hsla,
    /// `--destructive`.
    pub danger: Hsla,
    pub warning: Hsla,
    pub success: Hsla,
    pub glass_1: Hsla,
    pub glass_2: Hsla,
    pub glass_3: Hsla,
    /// `--ls-glass-edge`.
    pub glass_edge: Hsla,
    /// The 1px inset highlight on top of floating glass.
    pub glass_highlight: Hsla,
    /// `--ls-glass-opaque` (`--popover`).
    pub glass_opaque: Hsla,
    /// `--ls-scrim`.
    pub scrim: Hsla,
    /// `--ls-accent`.
    pub accent: Hsla,
    /// `--primary`: fills that carry text (the send button, primary buttons).
    pub accent_fill: Hsla,
    /// `--message-action-hover`.
    pub accent_hover: Hsla,
    /// `--primary` as text (links, accented labels).
    pub accent_text: Hsla,
    /// `--accent`: soft accent fills (selected rows, the user's bubble).
    pub accent_soft: Hsla,
    /// `--ring`.
    pub accent_ring: Hsla,
    /// `--sidebar-row-hover`: a row under the pointer.
    pub hover: Hsla,
    pub shadow: Hsla,

    /// `--muted` (`--secondary`): neutral soft fills.
    pub muted: Hsla,
    /// `--sidebar-icon-color`.
    pub sidebar_icon: Hsla,
    /// `--sidebar-row-active`: the thread on screen.
    pub sidebar_row_active: Hsla,
    /// `--sidebar-row-selected`.
    pub sidebar_row_selected: Hsla,
    /// `--sidebar-control-surface`.
    pub sidebar_control: Hsla,
    /// The top bar: glass 1 over the work, blurred (measured on the web, it is one color).
    pub header_bg: Hsla,
    /// `--message-surface`: the user's bubble.
    pub message_surface: Hsla,
    /// `--code-background`.
    pub code_bg: Hsla,
    /// `--info` and `--info-foreground`.
    pub info: Hsla,
    pub info_text: Hsla,
    /// `--warning-foreground`, `--success-foreground`, `--destructive-foreground`.
    pub warning_text: Hsla,
    pub success_text: Hsla,
    pub danger_text: Hsla,
    /// `--warning-surface`, `--error-surface`.
    pub warning_surface: Hsla,
    pub danger_surface: Hsla,
    /// `--diff-addition-foreground`, `--diff-deletion-foreground`.
    pub diff_added: Hsla,
    pub diff_removed: Hsla,
    /// `--terminal-selection-background`.
    pub selection: Hsla,
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub mode: Mode,
    pub colors: Colors,
    /// "Reduce transparency" is on: opaque chrome, no glass.
    pub reduce_transparency: bool,
}

impl Global for Theme {}

/// The web's type scale (Tailwind, px): xs 12/16 · sm 14/20 · base 16/24; the window's names
/// are kept (`BASE` was its control size, 13 before).
pub mod text {
    pub const XS: f32 = 12.;
    pub const SM: f32 = 12.;
    pub const BASE: f32 = 14.;
    pub const MD: f32 = 14.;
    pub const LG: f32 = 16.;
    pub const XL: f32 = 20.;
    pub const XXL: f32 = 30.;
}

#[allow(dead_code)]
/// Radii (px), Tailwind's with `--radius: .625rem`: sm 6 · md 8 · lg 10 · xl 14 · 2xl 18 ·
/// 3xl 22.
pub mod radius {
    pub const XS: f32 = 4.;
    pub const SM: f32 = 6.;
    pub const MD: f32 = 8.;
    pub const LG: f32 = 10.;
    pub const XL: f32 = 14.;
    pub const XXL: f32 = 18.;
    pub const XXXL: f32 = 22.;
}

#[derive(Deserialize)]
struct WebTheme {
    colors: std::collections::HashMap<String, ByMode>,
}

#[derive(Deserialize)]
struct ByMode {
    light: String,
    dark: String,
}

/// `#rrggbb`, `#rrggbbaa` or `rgba(r,g,b,a)`.
pub fn parse_color(value: &str) -> Hsla {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        let n = u32::from_str_radix(hex, 16).unwrap_or(0);
        let (rgb, a) = if hex.len() == 8 { (n >> 8, (n & 0xff) as f32 / 255.) } else { (n, 1.) };
        return Rgba {
            r: ((rgb >> 16) & 0xff) as f32 / 255.,
            g: ((rgb >> 8) & 0xff) as f32 / 255.,
            b: (rgb & 0xff) as f32 / 255.,
            a,
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

impl Theme {
    pub fn new(mode: Mode, reduce_transparency: bool) -> Self {
        let web: WebTheme = serde_json::from_str(WEB_THEME).expect("assets/web-theme.json");
        let role = |name: &str| {
            let colors = web.colors.get(name).unwrap_or_else(|| panic!("assets/web-theme.json has no {name}"));
            parse_color(match mode {
                Mode::Dark => &colors.dark,
                Mode::Light => &colors.light,
            })
        };
        let dark = mode == Mode::Dark;
        let opaque = role("ls-glass-opaque");
        let tier = |name: &str| if reduce_transparency { opaque } else { role(name) };
        Self {
            mode,
            reduce_transparency,
            colors: Colors {
                bg: role("ls-bg"),
                bg_raised: role("background"),
                bg_sunken: role("card"),
                text: role("foreground"),
                text_2: role("muted-foreground"),
                text_3: role("ls-text-3"),
                text_on_accent: role("primary-foreground"),
                line: role("border"),
                line_strong: role("input"),
                danger: role("destructive"),
                warning: role("warning"),
                success: role("success"),
                glass_1: tier("lsg-1-bg"),
                glass_2: tier("lsg-2-bg"),
                glass_3: tier("lsg-3-bg"),
                glass_edge: role("ls-glass-edge"),
                glass_highlight: parse_color(if dark { "#ffffff12" } else { "#ffffffe6" }),
                glass_opaque: opaque,
                scrim: role("ls-scrim"),
                accent: role("ls-accent"),
                accent_fill: role("primary"),
                accent_hover: role("message-action-hover"),
                accent_text: role("primary"),
                accent_soft: role("accent"),
                accent_ring: role("ring"),
                hover: role("sidebar-row-hover"),
                // --ls-glass-shadow: 0 12px 40px, then 0 1px 2px.
                shadow: parse_color(if dark { "#00000073" } else { "#141e3224" }),
                muted: role("muted"),
                sidebar_icon: role("sidebar-icon-color"),
                sidebar_row_active: role("sidebar-row-active"),
                sidebar_row_selected: role("sidebar-row-selected"),
                sidebar_control: role("sidebar-control-surface"),
                header_bg: if reduce_transparency {
                    opaque
                } else {
                    parse_color(if dark { "#15171f" } else { "#fbfcfe" })
                },
                message_surface: role("message-surface"),
                code_bg: role("code-background"),
                info: role("info"),
                info_text: role("info-foreground"),
                warning_text: role("warning-foreground"),
                success_text: role("success-foreground"),
                danger_text: role("destructive-foreground"),
                warning_surface: role("warning-surface"),
                danger_surface: role("error-surface"),
                diff_added: role("diff-addition-foreground"),
                diff_removed: role("diff-deletion-foreground"),
                selection: role("terminal-selection-background"),
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
                color: parse_color(if self.mode == Mode::Dark { "#00000066" } else { "#141e321a" }),
                offset: gpui::point(px(0.), px(1.)),
                blur_radius: px(2.),
                spread_radius: px(0.),
            },
        ]
    }

    /// Floating glass (glass 2): GPUI cannot blur inside the window, so menus and the palette
    /// sit on the tier's opaque fallback, the web's `--popover`.
    pub fn floating_bg(&self) -> Hsla {
        self.colors.glass_opaque
    }

    /// `text-{name}-{light} dark:text-{name}-{dark}` (with the dark shade's alpha, e.g.
    /// `dark:text-emerald-300/90`).
    pub fn tw(&self, name: &str, light: u16, dark: u16, dark_alpha: f32) -> Hsla {
        match self.mode {
            Mode::Light => tw::color(name, light),
            Mode::Dark => tw::color(name, dark).opacity(dark_alpha),
        }
    }

    /// The sidebar's material (`assets/material-*.png`), unless transparency is reduced.
    pub fn material(&self) -> Option<&'static str> {
        (!self.reduce_transparency).then_some(match self.mode {
            Mode::Dark => "material-dark.png",
            Mode::Light => "material-light.png",
        })
    }
}

/// The Tailwind colors the web's components name directly (Tailwind 4.3.3, shades 300 to 700,
/// as Chromium draws them in sRGB): status labels, pull request states, project badges.
pub mod tw {
    use gpui::Hsla;

    const PALETTE: &[(&str, [&str; 5])] = &[
        ("gray", ["#d1d5dc", "#99a1af", "#6a7282", "#4a5565", "#364153"]),
        ("red", ["#ffa2a2", "#ff6467", "#fb2c36", "#e7000b", "#c10007"]),
        ("orange", ["#ffb86a", "#ff8904", "#ff6900", "#f54900", "#ca3500"]),
        ("amber", ["#ffd230", "#ffb900", "#fe9a00", "#e17100", "#bb4d00"]),
        ("yellow", ["#ffdf20", "#fdc700", "#f0b100", "#d08700", "#a65f00"]),
        ("lime", ["#bbf451", "#9ae600", "#7ccf00", "#5ea500", "#497d00"]),
        ("green", ["#7bf1a8", "#05df72", "#00c950", "#00a63e", "#008236"]),
        ("emerald", ["#5ee9b5", "#00d492", "#00bc7d", "#009966", "#007a55"]),
        ("teal", ["#46ecd5", "#00d5be", "#00bba7", "#009689", "#00786f"]),
        ("cyan", ["#53eafd", "#00d3f2", "#00b8db", "#0092b8", "#007595"]),
        ("sky", ["#74d4ff", "#00bcff", "#00a6f4", "#0084d1", "#0069a8"]),
        ("blue", ["#8ec5ff", "#51a2ff", "#2b7fff", "#155dfc", "#1447e6"]),
        ("indigo", ["#a3b3ff", "#7c86ff", "#615fff", "#4f39f6", "#432dd7"]),
        ("violet", ["#c4b4ff", "#a684ff", "#8e51ff", "#7f22fe", "#7008e7"]),
        ("purple", ["#dab2ff", "#c27aff", "#ad46ff", "#9810fa", "#8200db"]),
        ("fuchsia", ["#f4a8ff", "#ed6aff", "#e12afb", "#c800de", "#a800b7"]),
        ("pink", ["#fda5d5", "#fb64b6", "#f6339a", "#e60076", "#c6005c"]),
        ("rose", ["#ffa1ad", "#ff637e", "#ff2056", "#ec003f", "#c70036"]),
        ("zinc", ["#d4d4d8", "#9f9fa9", "#71717b", "#52525c", "#3f3f46"]),
    ];

    /// `text-{color}-{shade}`; shade is 300, 400, 500, 600 or 700.
    pub fn color(name: &str, shade: u16) -> Hsla {
        let index = match shade {
            300 => 0,
            400 => 1,
            500 => 2,
            600 => 3,
            _ => 4,
        };
        PALETTE
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, shades)| super::parse_color(shades[index]))
            .unwrap_or_else(gpui::black)
    }

    /// The project badge colors, in the web's order (`projectIconColors.ts`).
    pub const PROJECT_COLORS: [&str; 18] = [
        "gray", "red", "orange", "amber", "yellow", "lime", "green", "emerald", "teal", "cyan", "sky", "blue", "indigo", "violet", "purple", "fuchsia", "pink",
        "rose",
    ];
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

    /// Text stays readable where it sits (DESIGN.md "Accessibility"): ≥ 4.5 for text and
    /// secondary text on the work, the sidebar (glass 1 over the backdrop) and floating
    /// surfaces, in both modes.
    #[test]
    fn text_keeps_contrast() {
        for mode in [Mode::Dark, Mode::Light] {
            let theme = Theme::new(mode, false);
            let c = &theme.colors;
            let sidebar = over(c.glass_1, c.bg);
            for surface in [
                c.bg_raised,
                c.bg_sunken,
                c.glass_opaque,
                c.header_bg,
                sidebar,
                over(c.sidebar_row_active, sidebar),
            ] {
                assert!(contrast(c.text, surface) >= 4.5, "{mode:?} text {}", contrast(c.text, surface));
                assert!(contrast(c.text_2, surface) >= 4.5, "{mode:?} text-2 {}", contrast(c.text_2, surface));
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
        let soft = parse_color("#4777d224");
        assert!((soft.a - 0x24 as f32 / 255.).abs() < 0.001);
        let reduced = Theme::new(Mode::Dark, true);
        assert_eq!(reduced.colors.glass_1, reduced.colors.glass_opaque);
    }
}
