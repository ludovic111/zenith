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
    AlarmClock => "alarm-clock",
    AlarmClockOff => "alarm-clock-off",
    Archive => "archive",
    ArchiveRestore => "archive-restore",
    Pull => "arrow-down-to-line",
    ArrowLeft => "arrow-left",
    ArrowUp => "arrow-up",
    Push => "arrow-up-from-line",
    Bell => "bell",
    Book => "book-open-text",
    Bot => "bot",
    Box => "box",
    Brain => "brain",
    Bug => "bug",
    ChartNoAxesColumn => "chart-no-axes-column",
    Check => "check",
    ChevronDown => "chevron-down",
    ChevronRight => "chevron-right",
    ChevronUp => "chevron-up",
    CircleAlert => "circle-alert",
    CircleCheck => "circle-check",
    CircleDashed => "circle-dashed",
    CircleDot => "circle-dot",
    CircleHelp => "circle-help",
    CircleStop => "circle-stop",
    Clock => "clock",
    CloudDownload => "cloud-download",
    CloudUpload => "cloud-upload",
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
    FlaskConical => "flask-conical",
    Folder => "folder",
    FolderClosed => "folder-closed",
    FolderCode => "folder-code",
    FolderGit => "folder-git",
    FolderGit2 => "folder-git-2",
    FolderOpen => "folder-open",
    FolderPlus => "folder-plus",
    Gauge => "gauge",
    GitBranch => "git-branch",
    GitBranchPlus => "git-branch-plus",
    GitCommit => "git-commit-horizontal",
    GitMerge => "git-merge",
    PullRequest => "git-pull-request",
    PullRequestArrow => "git-pull-request-arrow",
    PullRequestClosed => "git-pull-request-closed",
    PullRequestDraft => "git-pull-request-draft",
    GitHub => "github",
    Globe => "globe",
    Hammer => "hammer",
    Hand => "hand",
    History => "history",
    Info => "info",
    Layers => "layers",
    ListChecks => "list-checks",
    Loader => "loader-circle",
    Lock => "lock",
    LockOpen => "lock-open",
    MailOpen => "mail-open",
    Maximize2 => "maximize-2",
    MessageCircle => "message-circle",
    MessageCircleQuestion => "message-circle-question",
    Message => "message-square",
    MessageSquarePlus => "message-square-plus",
    Minimize2 => "minimize-2",
    Moon => "moon",
    MoonStar => "moon-star",
    PanelBottom => "panel-bottom",
    PanelLeft => "panel-left",
    PanelLeftClose => "panel-left-close",
    PanelLeftOpen => "panel-left-open",
    PanelRight => "panel-right",
    Paperclip => "paperclip",
    PenLine => "pen-line",
    Pencil => "pencil",
    PencilRuler => "pencil-ruler",
    Pin => "pin",
    PinOff => "pin-off",
    Play => "play",
    Plug => "plug",
    Plus => "plus",
    ProviderClaude => "provider-claude",
    ProviderCursor => "provider-cursor",
    ProviderGrok => "provider-grok",
    ProviderOpenAi => "provider-openai",
    Refresh => "refresh-cw",
    Revert => "rotate-ccw",
    Search => "search",
    Server => "server",
    Settings => "settings",
    Shield => "shield",
    ShieldQuestion => "shield-question",
    Smartphone => "smartphone",
    Sparkles => "sparkles",
    SquareArrowOutUpRight => "square-arrow-out-up-right",
    NewThread => "square-pen",
    SquareTerminal => "square-terminal",
    Sun => "sun",
    Terminal => "terminal",
    Timer => "timer",
    Trash => "trash-2",
    Warning => "triangle-alert",
    Undo2 => "undo-2",
    WrapText => "wrap-text",
    Wrench => "wrench",
    X => "x",
    Zap => "zap",
}

pub struct Assets;

/// Images that are not icons: the sidebar's material (see `theme`) and zenith's mark.
fn image_bytes(path: &str) -> Option<&'static [u8]> {
    match path {
        "material-light.png" => Some(include_bytes!("../assets/material-light.png")),
        "material-dark.png" => Some(include_bytes!("../assets/material-dark.png")),
        "zenith-mark.svg" => Some(include_bytes!("../assets/zenith-mark.svg")),
        _ => None,
    }
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(icon_bytes(path).or_else(|| image_bytes(path)).map(Cow::Borrowed))
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
        // The web only ships Plex Mono up to 600: its bold (the project monograms) renders as
        // this face, converted from the web interface's own file.
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexMono-SemiBold.ttf")),
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
