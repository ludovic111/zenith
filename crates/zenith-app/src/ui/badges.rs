//! Small marks the web interface puts next to names: a project's monogram, a provider's logo, a
//! thread's pull request.

use gpui::prelude::*;
use gpui::{div, px, svg, App, FontWeight, Hsla, SharedString, Svg};
use unicode_normalization::UnicodeNormalization;
use zenith_model::shell::{PullRequestBadge, PullRequestTone};

use crate::assets::{Icon, MONO_FONT};
use crate::theme::{tw, ActiveTheme};

/// A project's monogram and color, as `projectIdentity.ts` derives them from its name: the
/// first letter, then the first digit of the first word, else the first letter of the last
/// word, else the last letter of the first word.
pub fn project_identity(name: &str) -> (String, &'static str) {
    let name: String = name.nfkc().collect::<String>().trim().to_owned();
    let words: Vec<Vec<char>> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.chars().collect())
        .collect();
    let monogram = match words.first() {
        None => "PR".to_owned(),
        Some(first_word) => {
            let first = first_word[0];
            let second = first_word[1..]
                .iter()
                .copied()
                .find(|c| c.is_numeric())
                .or_else(|| {
                    if words.len() > 1 {
                        words.last().map(|w| w[0])
                    } else {
                        first_word.last().copied()
                    }
                })
                .unwrap_or(first);
            format!("{first}{second}").to_uppercase().chars().take(2).collect()
        }
    };
    let seed = if name.is_empty() { "project".to_owned() } else { name.to_lowercase() };
    let mut index: u32 = 0;
    for c in seed.chars() {
        index = (index * 31 + c as u32) % tw::PROJECT_COLORS.len() as u32;
    }
    (monogram, tw::PROJECT_COLORS[index as usize])
}

/// The monogram tile (`ProjectMonogram`): the color at 14% behind two IBM Plex Mono letters,
/// stretched over 12 px (6 for one), in a square rounded at a quarter of its side.
pub fn project_badge(name: &str, size: f32, cx: &App) -> gpui::Div {
    let (monogram, color) = project_identity(name);
    let fg: Hsla = cx.theme().tw(color, 600, 400, 1.);
    let scale = size / 16.;
    let glyphs: Vec<char> = monogram.chars().collect();
    // The SVG's text sits on a baseline at 10.8 of 16, 8.25 px high; each glyph gets a 6 px cell.
    let cell = 6. * scale;
    let font = 8.25 * scale;
    div()
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(size / 4.))
        .bg(fg.opacity(0.14))
        .text_color(fg)
        .font_family(MONO_FONT)
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(font))
        .line_height(px(size))
        .children(
            glyphs
                .into_iter()
                .map(move |g| div().w(px(cell)).flex().justify_center().child(SharedString::from(g.to_string()))),
        )
}

/// A provider's logo by its instance id (`claudeAgent`, `codex`…), and the color the web gives
/// it (Claude's own orange; the others follow the text).
pub fn provider_logo(provider: &str) -> (Icon, Option<Hsla>) {
    let p = provider.to_ascii_lowercase();
    if p.contains("claude") {
        (Icon::ProviderClaude, Some(crate::theme::parse_color("#d97757")))
    } else if p.contains("codex") || p.contains("openai") {
        (Icon::ProviderOpenAi, None)
    } else if p.contains("cursor") {
        (Icon::ProviderCursor, None)
    } else if p.contains("grok") {
        (Icon::ProviderGrok, None)
    } else {
        (Icon::Bot, None)
    }
}

pub fn provider_icon(provider: &str, size: f32, cx: &App) -> Svg {
    let (icon, color) = provider_logo(provider);
    svg()
        .path(icon.path())
        .size(px(size))
        .flex_none()
        .text_color(color.unwrap_or(cx.theme().colors.text))
}

/// The glyph and color of a pull request badge (`PULL_REQUEST_STATE_PRESENTATION`).
pub fn pull_request_look(badge: &PullRequestBadge, cx: &App) -> (Icon, Hsla) {
    let theme = cx.theme();
    let (icon, color) = match badge.tone {
        PullRequestTone::Open => (Icon::PullRequestArrow, theme.tw("emerald", 600, 300, 0.9)),
        PullRequestTone::Draft => (Icon::PullRequestDraft, theme.tw("zinc", 500, 400, 0.8)),
        PullRequestTone::Closed => (Icon::PullRequestClosed, theme.tw("red", 600, 300, 0.9)),
        PullRequestTone::Merged => (Icon::GitMerge, theme.tw("violet", 600, 300, 0.9)),
        PullRequestTone::Unknown => (Icon::PullRequestArrow, theme.colors.text_2),
    };
    (if badge.stack { Icon::Layers } else { icon }, color)
}

/// The badge itself: a 12 px glyph and the number, 2 px apart, at 12 px in the state's color.
pub fn pull_request_badge(badge: &PullRequestBadge, cx: &App) -> gpui::Div {
    let (icon, color) = pull_request_look(badge, cx);
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(2.))
        .text_size(px(12.))
        .line_height(px(16.))
        .text_color(color)
        .child(svg().path(icon.path()).size(px(12.)).flex_none().text_color(color))
        .child(SharedString::from(badge.text.clone()))
}

#[cfg(test)]
mod tests {
    use super::project_identity;

    #[test]
    fn monograms_and_colors_follow_the_web() {
        assert_eq!(project_identity("zenith").0, "ZH");
        assert_eq!(project_identity("my project").0, "MP");
        assert_eq!(project_identity("app2 web").0, "A2");
        assert_eq!(project_identity("x").0, "XX");
        assert_eq!(project_identity("--").0, "PR");
        // h = (h*31 + c) % 18 over "zenith".
        let mut h = 0u32;
        for c in "zenith".chars() {
            h = (h * 31 + c as u32) % 18;
        }
        assert_eq!(project_identity("Zenith").1, crate::theme::tw::PROJECT_COLORS[h as usize]);
    }
}
