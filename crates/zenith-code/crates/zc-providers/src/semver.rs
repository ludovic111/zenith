//! Port of `packages/shared/src/semver.ts`: the small semver parser, comparator and range
//! checker the CLI gates and compatibility policies use (comparator groups joined by `||`).

use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSemver {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Vec<String>,
}

/// `normalizeSemverVersion`: pad `20` / `20.1` to three segments.
pub fn normalize_semver_version(version: &str) -> String {
    let trimmed = version.trim();
    let (main, prerelease) = match trimmed.split_once('-') {
        Some((main, rest)) => (main, Some(rest.split('-').next().unwrap_or(""))),
        None => (trimmed, None),
    };
    let mut segments: Vec<String> = main
        .split('.')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect();
    while !segments.is_empty() && segments.len() < 3 {
        segments.push("0".to_owned());
    }
    match prerelease {
        Some(prerelease) if !prerelease.is_empty() => format!("{}-{prerelease}", segments.join(".")),
        _ => segments.join("."),
    }
}

fn is_number_segment(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_digit())
}

/// `parseSemver`.
pub fn parse_semver(value: &str) -> Option<ParsedSemver> {
    let normalized = normalize_semver_version(value);
    let normalized = normalized.strip_prefix('v').unwrap_or(&normalized);
    let (main, prerelease) = match normalized.split_once('-') {
        Some((main, rest)) => (main, Some(rest.split('-').next().unwrap_or(""))),
        None => (normalized, None),
    };
    let segments: Vec<&str> = main.split('.').collect();
    if segments.len() != 3 || !segments.iter().all(|segment| is_number_segment(segment)) {
        return None;
    }
    Some(ParsedSemver {
        major: segments[0].parse().ok()?,
        minor: segments[1].parse().ok()?,
        patch: segments[2].parse().ok()?,
        prerelease: prerelease
            .map(|prerelease| {
                prerelease
                    .split('.')
                    .map(str::trim)
                    .filter(|segment| !segment.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

fn compare_prerelease_identifier(left: &str, right: &str) -> Ordering {
    match (is_number_segment(left), is_number_segment(right)) {
        (true, true) => left.parse::<u64>().unwrap_or(0).cmp(&right.parse::<u64>().unwrap_or(0)),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => left.cmp(right),
    }
}

/// `compareSemverVersions`: unparsable versions compare as strings.
pub fn compare_semver_versions(left: &str, right: &str) -> Ordering {
    let (Some(parsed_left), Some(parsed_right)) = (parse_semver(left), parse_semver(right)) else {
        return left.cmp(right);
    };
    let base = (parsed_left.major, parsed_left.minor, parsed_left.patch).cmp(&(parsed_right.major, parsed_right.minor, parsed_right.patch));
    if base != Ordering::Equal {
        return base;
    }
    match (parsed_left.prerelease.is_empty(), parsed_right.prerelease.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    let length = parsed_left.prerelease.len().max(parsed_right.prerelease.len());
    for index in 0..length {
        let (Some(left), Some(right)) = (parsed_left.prerelease.get(index), parsed_right.prerelease.get(index)) else {
            return if parsed_left.prerelease.get(index).is_none() {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        };
        let comparison = compare_prerelease_identifier(left, right);
        if comparison != Ordering::Equal {
            return comparison;
        }
    }
    Ordering::Equal
}

fn parse_loose_triplet(value: &str) -> Option<(u64, u64, u64)> {
    let mut parts = value.split('.');
    let major = parts.next()?;
    let minor = parts.next();
    let patch = parts.next();
    if parts.next().is_some() || !is_number_segment(major) {
        return None;
    }
    let number = |part: Option<&str>| -> Option<u64> {
        match part {
            None => Some(0),
            Some(text) if is_number_segment(text) => text.parse().ok(),
            Some(_) => None,
        }
    };
    Some((major.parse().ok()?, number(minor)?, number(patch)?))
}

/// `satisfiesSemverRange(version, range)`.
pub fn satisfies_semver_range(raw_version: &str, range: &str) -> bool {
    let trimmed = raw_version.trim();
    let normalized = trimmed.strip_prefix('v').unwrap_or(trimmed);
    // /^(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:-[0-9A-Za-z.-]+)?$/
    let (core, prerelease) = match normalized.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (normalized, None),
    };
    if let Some(prerelease) = prerelease {
        if prerelease.is_empty() || !prerelease.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
            return false;
        }
    }
    let Some(version) = parse_loose_triplet(core) else {
        return false;
    };
    range.split("||").any(|group| {
        let comparators: Vec<&str> = group.split_whitespace().collect();
        if comparators.is_empty() {
            return false;
        }
        comparators.iter().all(|comparator| {
            let comparator = comparator.trim();
            let (operator, rest) = ["^", ">=", "<=", ">", "<", "="]
                .iter()
                .find_map(|operator| comparator.strip_prefix(operator).map(|rest| (*operator, rest)))
                .unwrap_or(("=", comparator));
            let rest = rest.trim_start();
            let rest = rest.strip_prefix('v').unwrap_or(rest);
            let Some(target) = parse_loose_triplet(rest) else {
                return false;
            };
            let compared = version.cmp(&target);
            match operator {
                "^" => {
                    if compared == Ordering::Less {
                        false
                    } else if target.0 > 0 {
                        version.0 == target.0
                    } else if target.1 > 0 {
                        version.0 == 0 && version.1 == target.1
                    } else {
                        version.0 == 0 && version.1 == 0 && version.2 == target.2
                    }
                }
                ">=" => compared != Ordering::Less,
                ">" => compared == Ordering::Greater,
                "<=" => compared != Ordering::Greater,
                "<" => compared == Ordering::Less,
                _ => compared == Ordering::Equal,
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_compares() {
        assert_eq!(parse_semver("v1.2.3-beta.1").unwrap().prerelease, vec!["beta", "1"]);
        assert_eq!(parse_semver("20").unwrap().major, 20);
        assert!(parse_semver("1.2.x").is_none());
        assert_eq!(compare_semver_versions("1.2.3", "1.2.4"), Ordering::Less);
        assert_eq!(compare_semver_versions("1.2.3", "1.2.3-rc.1"), Ordering::Greater);
        assert_eq!(compare_semver_versions("1.2.3-rc.2", "1.2.3-rc.10"), Ordering::Less);
    }

    #[test]
    fn checks_ranges() {
        assert!(satisfies_semver_range("2.1.280", ">=2.1.280 <3"));
        assert!(!satisfies_semver_range("2.1.279", ">=2.1.280"));
        assert!(satisfies_semver_range("0.0.43", "^0.0.43 || >=0.1.0"));
        assert!(satisfies_semver_range("1.4.0", "^1.2"));
        assert!(!satisfies_semver_range("2.0.0", "^1.2"));
        assert!(satisfies_semver_range("v1.2.3-beta", "1.2.3"));
        assert!(!satisfies_semver_range("1.2.3", ""));
        assert!(!satisfies_semver_range("nightly", ">=1"));
    }
}
