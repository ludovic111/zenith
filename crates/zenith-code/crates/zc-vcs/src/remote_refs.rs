//! Remote ref parsing (`apps/server/src/git/remoteRefs.ts`).

/// `parseRemoteNamesInGitOrder`: one trimmed, non-empty name per line of `git remote`.
pub fn parse_remote_names_in_git_order(stdout: &str) -> Vec<String> {
    stdout.split('\n').map(str::trim).filter(|name| !name.is_empty()).map(str::to_owned).collect()
}

/// `parseRemoteNames`: the names longest first (stable for equal lengths), so the longest
/// prefix wins when matching `remote/branch`.
pub fn parse_remote_names(stdout: &str) -> Vec<String> {
    let mut names = parse_remote_names_in_git_order(stdout);
    sort_longest_first(&mut names);
    names
}

/// `toSorted((a, b) => b.length - a.length)` (JS string length is UTF-16 units).
pub fn sort_longest_first(names: &mut [String]) {
    names.sort_by_key(|name| std::cmp::Reverse(name.encode_utf16().count()));
}

/// A `remote/branch` ref split on a known remote name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRemoteRef {
    pub remote_ref: String,
    pub remote_name: String,
    pub branch_name: String,
}

/// `parseRemoteRefWithRemoteNames`: the first remote (in the given order) whose `name/` prefixes
/// the trimmed ref.
pub fn parse_remote_ref_with_remote_names<S: AsRef<str>>(reference: &str, remote_names: &[S]) -> Option<ParsedRemoteRef> {
    let trimmed = reference.trim();
    if trimmed.is_empty() {
        return None;
    }
    for remote_name in remote_names {
        let remote_name = remote_name.as_ref();
        let prefix = format!("{remote_name}/");
        let Some(rest) = trimmed.strip_prefix(&prefix) else {
            continue;
        };
        let branch_name = rest.trim();
        if branch_name.is_empty() {
            return None;
        }
        return Some(ParsedRemoteRef {
            remote_ref: trimmed.to_owned(),
            remote_name: remote_name.to_owned(),
            branch_name: branch_name.to_owned(),
        });
    }
    None
}

/// `extractBranchNameFromRemoteRef`.
pub fn extract_branch_name_from_remote_ref(reference: &str, remote_name: Option<&str>, remote_names: &[String]) -> String {
    let normalized = reference.trim();
    if normalized.is_empty() {
        return String::new();
    }
    if let Some(rest) = normalized.strip_prefix("refs/remotes/") {
        return extract_branch_name_from_remote_ref(rest, remote_name, remote_names);
    }
    let parsed = match remote_name {
        Some(name) if !name.is_empty() => parse_remote_ref_with_remote_names(normalized, &[name]),
        _ => parse_remote_ref_with_remote_names(normalized, remote_names),
    };
    if let Some(parsed) = parsed {
        return parsed.branch_name;
    }
    match normalized.find('/') {
        None => normalized.to_owned(),
        Some(index) => normalized[index + 1..].trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_remote_name_wins() {
        let names = parse_remote_names("origin\nmy-org/upstream\n\n  fork \n");
        assert_eq!(names, vec!["my-org/upstream", "origin", "fork"]);
        let parsed = parse_remote_ref_with_remote_names("my-org/upstream/feature", &names).unwrap();
        assert_eq!(parsed.remote_name, "my-org/upstream");
        assert_eq!(parsed.branch_name, "feature");
        assert!(parse_remote_ref_with_remote_names("origin/", &names).is_none());
        assert!(parse_remote_ref_with_remote_names("main", &names).is_none());
    }

    #[test]
    fn extracts_branch_names() {
        let names = vec!["origin".to_owned()];
        assert_eq!(extract_branch_name_from_remote_ref("refs/remotes/origin/a/b", None, &names), "a/b");
        assert_eq!(extract_branch_name_from_remote_ref("other/x", None, &names), "x");
        assert_eq!(extract_branch_name_from_remote_ref("main", None, &names), "main");
        assert_eq!(extract_branch_name_from_remote_ref("fork/x", Some("fork"), &[]), "x");
    }
}
