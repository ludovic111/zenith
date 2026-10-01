//! `project/gitCloneProgress.ts`: one line of `git clone --progress` stderr.

use std::sync::OnceLock;

use regex::Regex;
use zc_contracts::ProjectCloneStage;

use crate::util::js_trim;

/// `GitCloneProgressLine`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCloneProgressLine {
    pub stage: ProjectCloneStage,
    pub percent: Option<u8>,
    /// Transfer detail after the count, e.g. `12.30 MiB | 5.00 MiB/s`.
    pub detail: Option<String>,
}

const STAGE_PREFIXES: [(&str, ProjectCloneStage); 7] = [
    ("remote: Enumerating objects", ProjectCloneStage::Counting),
    ("remote: Counting objects", ProjectCloneStage::Counting),
    ("remote: Compressing objects", ProjectCloneStage::Counting),
    ("Receiving objects", ProjectCloneStage::Receiving),
    ("Resolving deltas", ProjectCloneStage::Resolving),
    ("Updating files", ProjectCloneStage::Checkout),
    ("Checking out files", ProjectCloneStage::Checkout),
];

/// `parseGitCloneProgressLine`: git redraws each counter with a bare `\r`, so callers hand over
/// each redraw as its own line. Lines that are not counters return `None`.
pub fn parse_git_clone_progress_line(line: &str) -> Option<GitCloneProgressLine> {
    static PERCENT: OnceLock<Regex> = OnceLock::new();
    let trimmed = js_trim(line);
    let stage = STAGE_PREFIXES.iter().find(|(prefix, _)| trimmed.starts_with(prefix))?.1;
    let percent = PERCENT.get_or_init(|| Regex::new(r":\s+([0-9]+)%\s+\(([0-9]+)/([0-9]+)\)(?:,\s*(.*?))?\s*(?:,\s*done\.)?\s*$").expect("valid regex"));
    let Some(captures) = percent.captures(trimmed) else {
        return Some(GitCloneProgressLine {
            stage,
            percent: None,
            detail: None,
        });
    };
    let value: Option<f64> = captures.get(1).and_then(|m| m.as_str().parse().ok());
    let raw_detail = captures.get(4).map(|m| js_trim(m.as_str())).unwrap_or_default();
    // The trailer of a finished line is "done." which carries no information.
    let detail = (!raw_detail.is_empty() && raw_detail != "done.").then(|| raw_detail.to_owned());
    Some(GitCloneProgressLine {
        stage,
        percent: value.filter(|v| v.is_finite()).map(|v| v.clamp(0.0, 100.0) as u8),
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_counters_and_details() {
        let line = parse_git_clone_progress_line("Receiving objects:  40% (4/10), 1.00 MiB | 2.00 MiB/s").unwrap();
        assert_eq!(line.stage, ProjectCloneStage::Receiving);
        assert_eq!(line.percent, Some(40));
        assert_eq!(line.detail.as_deref(), Some("1.00 MiB | 2.00 MiB/s"));
        let done = parse_git_clone_progress_line("Receiving objects: 100% (10/10), 2.50 MiB | 2.00 MiB/s, done.").unwrap();
        assert_eq!(done.detail.as_deref(), Some("2.50 MiB | 2.00 MiB/s"));
        let counting = parse_git_clone_progress_line("remote: Enumerating objects: 10, done.").unwrap();
        assert_eq!((counting.stage, counting.percent), (ProjectCloneStage::Counting, None));
        assert_eq!(parse_git_clone_progress_line("Resolving deltas: 100% (3/3), done.").unwrap().detail, None);
        assert!(parse_git_clone_progress_line("fatal: early EOF").is_none());
    }
}
