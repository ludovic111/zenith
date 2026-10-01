//! `pullRequest/pullRequestChecks.ts`: one row per check rather than one per run of it.

use zc_contracts::PullRequestCheck;

/// One run of a check as a host lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckEntry {
    pub check: PullRequestCheck,
    pub workflow_name: Option<String>,
    pub at: Option<String>,
}

/// ISO-8601 timestamps in UTC compare correctly as plain text.
fn is_at_least_as_new(candidate: Option<&str>, kept: Option<&str>) -> bool {
    match candidate {
        None => kept.is_none(),
        Some(candidate) => kept.is_none_or(|kept| candidate >= kept),
    }
}

/// `dedupeChecks`: the newest run of each check (keyed by workflow and name), at the place the
/// check first appeared; survivors sharing a name are shown as `workflow / name`.
pub fn dedupe_checks(entries: &[CheckEntry]) -> Vec<PullRequestCheck> {
    let mut keys: Vec<String> = Vec::new();
    let mut newest: Vec<&CheckEntry> = Vec::new();
    for entry in entries {
        let key = format!("{} {}", entry.workflow_name.as_deref().unwrap_or(""), entry.check.name);
        match keys.iter().position(|existing| *existing == key) {
            Some(index) => {
                if is_at_least_as_new(entry.at.as_deref(), newest[index].at.as_deref()) {
                    newest[index] = entry;
                }
            }
            None => {
                keys.push(key);
                newest.push(entry);
            }
        }
    }
    newest
        .iter()
        .map(|entry| {
            let workflow_name = entry.workflow_name.as_deref().unwrap_or("");
            let same_name = newest.iter().filter(|other| other.check.name == entry.check.name).count();
            if !workflow_name.is_empty() && same_name > 1 {
                PullRequestCheck {
                    name: format!("{workflow_name} / {}", entry.check.name),
                    ..entry.check.clone()
                }
            } else {
                entry.check.clone()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zc_contracts::PullRequestCheckStatus as S;

    fn entry(name: &str, status: S, workflow_name: Option<&str>, at: Option<&str>) -> CheckEntry {
        CheckEntry {
            check: PullRequestCheck {
                name: name.into(),
                status,
                description: None,
                url: None,
            },
            workflow_name: workflow_name.map(Into::into),
            at: at.map(Into::into),
        }
    }

    fn pairs(checks: &[PullRequestCheck]) -> Vec<(String, S)> {
        checks.iter().map(|check| (check.name.clone(), check.status)).collect()
    }

    #[test]
    fn keeps_the_newest_run_of_a_check_the_host_listed_twice() {
        let checks = dedupe_checks(&[
            entry("Prepare PR size config", S::Success, Some("PR Size"), Some("2026-08-11T16:06:25Z")),
            entry("Prepare PR size config", S::Pending, Some("PR Size"), Some("2026-08-11T17:01:04Z")),
        ]);
        assert_eq!(pairs(&checks), vec![("Prepare PR size config".into(), S::Pending)]);
    }

    #[test]
    fn holds_a_check_at_the_place_it_first_appeared() {
        let checks = dedupe_checks(&[
            entry("lint", S::Success, Some("CI"), Some("2026-08-11T16:00:00Z")),
            entry("test", S::Success, Some("CI"), Some("2026-08-11T16:00:00Z")),
            entry("lint", S::Failure, Some("CI"), Some("2026-08-11T18:00:00Z")),
        ]);
        assert_eq!(pairs(&checks), vec![("lint".into(), S::Failure), ("test".into(), S::Success)]);
    }

    #[test]
    fn loses_an_undated_run_to_a_dated_one_whichever_came_first() {
        let undated = dedupe_checks(&[
            entry("build", S::Success, None, Some("2026-08-11T16:00:00Z")),
            entry("build", S::Pending, None, None),
        ]);
        let dated = dedupe_checks(&[
            entry("build", S::Pending, None, None),
            entry("build", S::Success, None, Some("2026-08-11T16:00:00Z")),
        ]);
        assert_eq!((undated[0].status, dated[0].status), (S::Success, S::Success));
    }

    #[test]
    fn takes_the_last_copy_when_neither_run_is_dated() {
        let checks = dedupe_checks(&[entry("build", S::Pending, None, None), entry("build", S::Failure, None, None)]);
        assert_eq!(checks.iter().map(|check| check.status).collect::<Vec<_>>(), vec![S::Failure]);
    }

    #[test]
    fn keeps_two_workflows_that_name_a_job_the_same_thing() {
        let checks = dedupe_checks(&[
            entry("build", S::Success, Some("CI"), Some("2026-08-11T16:00:00Z")),
            entry("build", S::Failure, Some("Release"), Some("2026-08-11T16:00:00Z")),
        ]);
        assert_eq!(pairs(&checks), vec![("CI / build".into(), S::Success), ("Release / build".into(), S::Failure)]);
    }

    #[test]
    fn leaves_a_colliding_check_with_no_workflow_unqualified() {
        let checks = dedupe_checks(&[
            entry("build", S::Success, Some(""), Some("2026-08-11T16:00:00Z")),
            entry("build", S::Failure, Some("CI"), Some("2026-08-11T16:00:00Z")),
        ]);
        assert_eq!(
            checks.iter().map(|check| check.name.clone()).collect::<Vec<_>>(),
            vec!["build".to_owned(), "CI / build".to_owned()]
        );
    }
}
