//! `packages/shared/src/semver.ts` (`parseSemver`, `compareSemverVersions`): the comparison the
//! Claude model catalog uses for its CLI-version compatibility ranges.

use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSemver {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Vec<String>,
}

fn is_number_segment(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

/// JS `split(sep, 2)`: at most two pieces, the rest dropped.
fn split_two(value: &str, sep: char) -> (&str, Option<&str>) {
    match value.find(sep) {
        Some(index) => {
            let rest = &value[index + sep.len_utf8()..];
            let second = rest.find(sep).map_or(rest, |end| &rest[..end]);
            (&value[..index], Some(second))
        }
        None => (value, None),
    }
}

/// `normalizeSemverVersion`.
pub fn normalize_semver_version(version: &str) -> String {
    let (main, prerelease) = split_two(version.trim(), '-');
    let mut segments: Vec<&str> = main.split('.').map(str::trim).filter(|segment| !segment.is_empty()).collect();
    while !segments.is_empty() && segments.len() < 3 {
        segments.push("0");
    }
    match prerelease {
        Some(prerelease) if !prerelease.is_empty() => format!("{}-{prerelease}", segments.join(".")),
        _ => segments.join("."),
    }
}

/// `parseSemver`.
pub fn parse_semver(value: &str) -> Option<ParsedSemver> {
    let normalized = normalize_semver_version(value);
    let normalized = normalized.strip_prefix('v').unwrap_or(&normalized);
    let (main, prerelease) = split_two(normalized, '-');
    let segments: Vec<&str> = main.split('.').collect();
    if segments.len() != 3 || !segments.iter().all(|segment| is_number_segment(segment)) {
        return None;
    }
    let number = |segment: &str| segment.parse::<u64>().ok();
    Some(ParsedSemver {
        major: number(segments[0])?,
        minor: number(segments[1])?,
        patch: number(segments[2])?,
        prerelease: prerelease
            .map(|value| value.split('.').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
            .unwrap_or_default(),
    })
}

fn compare_prerelease_identifier(left: &str, right: &str) -> Ordering {
    match (is_number_segment(left), is_number_segment(right)) {
        (true, true) => left.parse::<u128>().unwrap_or(0).cmp(&right.parse::<u128>().unwrap_or(0)),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => left.cmp(right),
    }
}

/// `compareSemverVersions` (sign only).
pub fn compare_semver_versions(left: &str, right: &str) -> Ordering {
    let (Some(l), Some(r)) = (parse_semver(left), parse_semver(right)) else {
        return left.cmp(right);
    };
    l.major
        .cmp(&r.major)
        .then(l.minor.cmp(&r.minor))
        .then(l.patch.cmp(&r.patch))
        .then_with(|| match (l.prerelease.is_empty(), r.prerelease.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                for index in 0..l.prerelease.len().max(r.prerelease.len()) {
                    match (l.prerelease.get(index), r.prerelease.get(index)) {
                        (None, _) => return Ordering::Less,
                        (_, None) => return Ordering::Greater,
                        (Some(a), Some(b)) => {
                            let ordering = compare_prerelease_identifier(a, b);
                            if ordering != Ordering::Equal {
                                return ordering;
                            }
                        }
                    }
                }
                Ordering::Equal
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_like_the_shared_helper() {
        assert_eq!(compare_semver_versions("2.1.276", "2.1.280"), Ordering::Less);
        assert_eq!(compare_semver_versions("v3", "3.0.0"), Ordering::Equal);
        assert_eq!(compare_semver_versions("1.0.0-beta.2", "1.0.0-beta.10"), Ordering::Less);
        assert_eq!(compare_semver_versions("1.0.0", "1.0.0-rc.1"), Ordering::Greater);
        assert!(parse_semver("garbage").is_none());
        assert_eq!(parse_semver("20").map(|v| v.major), Some(20));
    }
}
