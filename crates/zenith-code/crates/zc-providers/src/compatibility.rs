//! Port of `provider/providerCompatibility.ts`: per-driver CLI version policies from the model
//! manifest, turned into `ServerProvider.compatibilityAdvisory`.

use serde::{Deserialize, Serialize};
use zc_contracts::{ServerProvider, ServerProviderCompatibilityAdvisory, ServerProviderCompatibilityStatus};

use crate::semver::satisfies_semver_range;

/// `packageJson.version` of `apps/server` (the T3 Code release the policies are keyed on).
pub const SERVER_PACKAGE_VERSION: &str = "0.0.43";

/// One `ranges` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompatibilityRange {
    pub range: String,
    pub status: ServerProviderCompatibilityStatus,
}

/// `ProviderCompatibilityPolicy`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCompatibilityPolicy {
    pub driver: String,
    pub t3_code_range: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_range: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_version: Option<String>,
    pub ranges: Vec<CompatibilityRange>,
}

fn is_version_range(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.split("||").all(|group| {
            let group = group.trim();
            // `group.trim().split(/\s+/)` on an empty group yields [""], which fails the token test.
            let tokens: Vec<&str> = if group.is_empty() { vec![""] } else { group.split_whitespace().collect() };
            tokens.iter().all(|token| is_range_token(token))
        })
}

/// `/^(?:\^|>=|>|<=|<|=)?v?\d+(?:\.\d+){0,2}$/`.
fn is_range_token(token: &str) -> bool {
    let rest = ["^", ">=", "<=", ">", "<", "="]
        .iter()
        .find_map(|operator| token.strip_prefix(operator))
        .unwrap_or(token);
    let rest = rest.strip_prefix('v').unwrap_or(rest);
    let parts: Vec<&str> = rest.split('.').collect();
    (1..=3).contains(&parts.len()) && parts.iter().all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

fn is_stable_version(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 3 && parts.iter().all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

impl ProviderCompatibilityPolicy {
    /// The schema checks of `ProviderCompatibilityPolicy` (ranges, stable recommendation, and a
    /// recommendation that lands in a supported range).
    pub fn is_valid(&self) -> bool {
        if self.driver.trim().is_empty() || !is_version_range(&self.t3_code_range) {
            return false;
        }
        if self.recommended_range.as_deref().is_some_and(|range| !is_version_range(range)) {
            return false;
        }
        if self.recommended_version.as_deref().is_some_and(|version| !is_stable_version(version.trim())) {
            return false;
        }
        if !self.ranges.iter().all(|entry| is_version_range(&entry.range)) {
            return false;
        }
        let Some(version) = &self.recommended_version else {
            return true;
        };
        self.recommended_range.as_deref().is_none_or(|range| satisfies_semver_range(version, range))
            && self
                .ranges
                .iter()
                .find(|entry| satisfies_semver_range(version, &entry.range))
                .map(|entry| entry.status)
                == Some(ServerProviderCompatibilityStatus::Supported)
    }
}

fn strip_cursor_build(version: &str) -> String {
    // /^(\d{4}\.\d{2}\.\d{2})-[a-f0-9]+$/
    if let Some((date, hash)) = version.split_once('-') {
        let date_ok =
            date.len() == 10 && date.split('.').map(str::len).collect::<Vec<_>>() == [4, 2, 2] && date.chars().all(|c| c.is_ascii_digit() || c == '.');
        if date_ok && !hash.is_empty() && hash.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)) {
            return date.to_owned();
        }
    }
    version.to_owned()
}

fn strip_antigravity_prefix(version: &str) -> String {
    match version.strip_prefix("agy_acp_server_") {
        Some(rest) if is_stable_version(rest) => rest.to_owned(),
        _ => version.to_owned(),
    }
}

/// `resolveProviderCompatibility(policies, driver, version, t3CodeVersion)`.
pub fn resolve_provider_compatibility(
    policies: Option<&[ProviderCompatibilityPolicy]>,
    driver: &str,
    version: Option<&str>,
    t3_code_version: &str,
) -> Option<ServerProviderCompatibilityAdvisory> {
    let policy = policies?
        .iter()
        .find(|entry| entry.driver == driver && satisfies_semver_range(t3_code_version, &entry.t3_code_range))?;
    let unprefixed = version.map(|version| version.strip_prefix('v').unwrap_or(version).to_owned());
    let stable = unprefixed.map(|value| match driver {
        "cursor" => strip_cursor_build(&value),
        "antigravity" => strip_antigravity_prefix(&value),
        _ => value,
    });
    let status = match &stable {
        Some(stable) if is_stable_version(stable) => policy
            .ranges
            .iter()
            .find(|entry| satisfies_semver_range(stable, &entry.range))
            .map(|entry| entry.status)
            .unwrap_or(ServerProviderCompatibilityStatus::Unknown),
        _ => ServerProviderCompatibilityStatus::Unknown,
    };
    let message = match status {
        ServerProviderCompatibilityStatus::Broken => Some("This provider version is known to be incompatible with this T3 Code release."),
        ServerProviderCompatibilityStatus::Unsupported => Some("This provider version is outside the supported range for this T3 Code release."),
        ServerProviderCompatibilityStatus::Graceful => Some("This provider version has limited compatibility with this T3 Code release."),
        _ => None,
    };
    let recommended_version = policy.recommended_version.clone();
    let recommended_range = policy.recommended_range.clone();
    let recommendation = recommended_version.clone().or_else(|| recommended_range.clone());
    let message = match (message, recommendation) {
        (Some(message), Some(recommendation)) => Some(format!("{message} Use {recommendation}.")),
        (message, _) => message.map(str::to_owned),
    };
    Some(ServerProviderCompatibilityAdvisory {
        status,
        latest_version_status: None,
        message,
        recommended_version,
        recommended_range,
    })
}

/// `applyProviderCompatibility(snapshot, policies, fallback)`: a remote policy replaces its
/// matching bundled policy; omission keeps the bundle.
pub fn apply_provider_compatibility(
    snapshot: &ServerProvider,
    policies: Option<&[ProviderCompatibilityPolicy]>,
    fallback: Option<&[ProviderCompatibilityPolicy]>,
) -> ServerProvider {
    let mut base = snapshot.clone();
    base.compatibility_advisory = None;
    if !snapshot.enabled || !snapshot.installed {
        return base;
    }
    let driver = snapshot.driver.as_str();
    let resolve = |version: Option<&str>| {
        resolve_provider_compatibility(policies, driver, version, SERVER_PACKAGE_VERSION)
            .or_else(|| resolve_provider_compatibility(fallback, driver, version, SERVER_PACKAGE_VERSION))
    };
    let advisory = resolve(snapshot.version.as_deref());
    let latest_version = snapshot
        .version_advisory
        .as_ref()
        .and_then(|advisory| advisory.latest_version.clone())
        .filter(|latest| !latest.is_empty());
    let latest_advisory = latest_version.as_deref().and_then(|latest| resolve(Some(latest)));
    if let Some(mut advisory) = advisory {
        advisory.latest_version_status = latest_advisory.map(|latest| latest.status);
        base.compatibility_advisory = Some(advisory);
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(driver: &str) -> ProviderCompatibilityPolicy {
        ProviderCompatibilityPolicy {
            driver: driver.into(),
            t3_code_range: ">=0.0.1".into(),
            recommended_range: Some(">=2.0.0".into()),
            recommended_version: Some("2.1.0".into()),
            ranges: vec![
                CompatibilityRange {
                    range: "<1.0.0".into(),
                    status: ServerProviderCompatibilityStatus::Broken,
                },
                CompatibilityRange {
                    range: ">=1.0.0 <2.0.0".into(),
                    status: ServerProviderCompatibilityStatus::Graceful,
                },
                CompatibilityRange {
                    range: ">=2.0.0".into(),
                    status: ServerProviderCompatibilityStatus::Supported,
                },
            ],
        }
    }

    #[test]
    fn classifies_boundaries_and_unknown_tags() {
        let policies = vec![policy("codex")];
        let resolve = |version: &str| resolve_provider_compatibility(Some(&policies), "codex", Some(version), SERVER_PACKAGE_VERSION).unwrap();
        assert_eq!(resolve("0.9.9").status, ServerProviderCompatibilityStatus::Broken);
        assert_eq!(
            resolve("0.9.9").message.as_deref(),
            Some("This provider version is known to be incompatible with this T3 Code release. Use 2.1.0.")
        );
        assert_eq!(resolve("1.0.0").status, ServerProviderCompatibilityStatus::Graceful);
        assert_eq!(resolve("v2.0.0").status, ServerProviderCompatibilityStatus::Supported);
        assert_eq!(resolve("2.0.0").message, None);
        assert_eq!(resolve("2.0.0-beta.1").status, ServerProviderCompatibilityStatus::Unknown);
        assert!(resolve_provider_compatibility(Some(&policies), "grok", Some("1.0.0"), SERVER_PACKAGE_VERSION).is_none());
    }

    #[test]
    fn strips_cursor_and_antigravity_release_forms() {
        let mut cursor = policy("cursor");
        cursor.ranges = vec![CompatibilityRange {
            range: ">=2026.1.1".into(),
            status: ServerProviderCompatibilityStatus::Supported,
        }];
        cursor.recommended_range = None;
        cursor.recommended_version = None;
        let policies = vec![cursor];
        let status = |version: &str| {
            resolve_provider_compatibility(Some(&policies), "cursor", Some(version), SERVER_PACKAGE_VERSION)
                .unwrap()
                .status
        };
        assert_eq!(status("2026.02.03-abc123"), ServerProviderCompatibilityStatus::Supported);
        assert_eq!(status("2026.02.03-beta"), ServerProviderCompatibilityStatus::Unknown);
        assert_eq!(strip_antigravity_prefix("agy_acp_server_1.2.3"), "1.2.3");
        assert_eq!(strip_antigravity_prefix("agy_acp_server_1.2"), "agy_acp_server_1.2");
    }

    #[test]
    fn rejects_invalid_policies() {
        assert!(policy("codex").is_valid());
        let mut outside = policy("codex");
        outside.recommended_version = Some("1.5.0".into());
        assert!(!outside.is_valid());
        let mut bad_range = policy("codex");
        bad_range.t3_code_range = ">=x".into();
        assert!(!bad_range.is_valid());
    }
}
