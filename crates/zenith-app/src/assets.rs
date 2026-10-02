//! What the app carries inside its binary: the lsuite fonts (Manrope for the interface, IBM
//! Plex Mono for code, both OFL) and Lucide icons (ISC), so it never fetches anything to draw.

use std::borrow::Cow;

use gpui::{App, AssetSource, SharedString};

macro_rules! icons {
    ($($variant:ident => $file:literal,)*) => {
        /// An icon from `assets/icons` (Lucide): the set the window draws from.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[allow(dead_code)]
        pub enum Icon {
            $($variant,)*
        }

        impl Icon {
            pub fn path(self) -> &'static str {
                match self {
                    $(Self::$variant => concat!("icons/", $file, ".svg"),)*
                }
            }
        }

        fn icon_bytes(path: &str) -> Option<&'static [u8]> {
            match path {
                $(concat!("icons/", $file, ".svg") => Some(include_bytes!(concat!("../assets/icons/", $file, ".svg"))),)*
                _ => None,
            }
        }

        const ICON_PATHS: &[&str] = &[$(concat!("icons/", $file, ".svg"),)*];
    };
}

icons! {
    ArchiveRestore => "archive-restore",
    Archive => "archive",
    ArrowUp => "arrow-up",
    Bell => "bell",
    Book => "book-open-text",
    Bot => "bot",
    Box => "box",
    Brain => "brain",
    Check => "check",
    ChevronDown => "chevron-down",
    ChevronRight => "chevron-right",
    ChevronUp => "chevron-up",
    CircleAlert => "circle-alert",
    CircleCheck => "circle-check",
    CircleDot => "circle-dot",
    CircleHelp => "circle-help",
    CircleStop => "circle-stop",
    Clock => "clock",
    Coins => "coins",
    Command => "command",
    Copy => "copy",
    Cpu => "cpu",
    Download => "download",
    Ellipsis => "ellipsis",
    ExternalLink => "external-link",
    Eye => "eye",
    FileDiff => "file-diff",
    FileEdit => "file-pen-line",
    FileSearch => "file-search",
    FileText => "file-text",
    FolderOpen => "folder-open",
    FolderPlus => "folder-plus",
    Folder => "folder",
    Gauge => "gauge",
    GitBranch => "git-branch",
    GitCommit => "git-commit-horizontal",
    GitMerge => "git-merge",
    Push => "arrow-up-from-line",
    Pull => "arrow-down-to-line",
    PullRequest => "git-pull-request",
    Globe => "globe",
    Hammer => "hammer",
    Hand => "hand",
    Info => "info",
    Layers => "layers",
    ListChecks => "list-checks",
    Loader => "loader-circle",
    Message => "message-square",
    MoonStar => "moon-star",
    Moon => "moon",
    PanelLeft => "panel-left",
    Paperclip => "paperclip",
    Pencil => "pencil",
    PinOff => "pin-off",
    Pin => "pin",
    Play => "play",
    Plug => "plug",
    Plus => "plus",
    Refresh => "refresh-cw",
    Revert => "rotate-ccw",
    Search => "search",
    Server => "server",
    Settings => "settings",
    Shield => "shield",
    Sparkles => "sparkles",
    NewThread => "square-pen",
    SquareTerminal => "square-terminal",
    Sun => "sun",
    Terminal => "terminal",
    Trash => "trash-2",
    Warning => "triangle-alert",
    Wrench => "wrench",
    X => "x",
    Zap => "zap",
}

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(icon_bytes(path).map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(ICON_PATHS.iter().filter(|p| p.starts_with(path)).map(|p| SharedString::from(*p)).collect())
    }
}

/// The interface font and the code font, as the theme names them.
pub const UI_FONT: &str = "Manrope";
pub const MONO_FONT: &str = "IBM Plex Mono";

/// Registers the bundled fonts with the text system.
pub fn load_fonts(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/Manrope-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/Manrope-Medium.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/Manrope-SemiBold.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/Manrope-Bold.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexMono-Medium.ttf")),
    ];
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        tracing::warn!(%error, "could not load the bundled fonts");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_is_bundled() {
        for path in ICON_PATHS {
            let bytes = Assets.load(path).unwrap().unwrap();
            assert!(std::str::from_utf8(&bytes).unwrap().contains("<svg"), "{path}");
        }
        assert_eq!(Icon::Plus.path(), "icons/plus.svg");
    }
}
