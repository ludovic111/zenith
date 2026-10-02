//! The git button of a thread's header, from the server's `vcs` status
//! (`apps/web/src/components/GitActionsControl.logic.ts`): the one action it offers
//! (`resolveQuickAction`) and its menu (`buildMenuItems`).

use serde_json::Value;

/// What the main half of the git button does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuickKind {
    /// Runs a stacked action (`commit`, `commit_push`, `commit_push_pr`, `push`, `create_pr`).
    Run(&'static str),
    Pull,
    OpenPr,
    Publish,
    /// Disabled, with the reason.
    Hint(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickAction {
    pub label: String,
    pub kind: QuickKind,
}

impl QuickAction {
    pub fn disabled(&self) -> bool {
        matches!(self.kind, QuickKind::Hint(_))
    }
}

/// "PR" on GitHub and most hosts, "MR" on GitLab, "change request" when the host is unknown.
pub fn change_request_label(status: Option<&Value>) -> &'static str {
    match status.and_then(|s| s.pointer("/sourceControlProvider/kind")).and_then(Value::as_str) {
        None => "PR",
        Some("gitlab") => "MR",
        Some("github" | "forgejo" | "azure-devops" | "bitbucket") => "PR",
        Some(_) => "change request",
    }
}

struct Facts {
    has_branch: bool,
    changes: bool,
    open_pr: bool,
    upstream: bool,
    remote: bool,
    default_ref: bool,
    ahead: i64,
    behind: i64,
    ahead_of_default: i64,
}

fn facts(status: &Value) -> Facts {
    let num = |key: &str| status.get(key).and_then(Value::as_i64).unwrap_or(0);
    let ahead = num("aheadCount");
    Facts {
        has_branch: status.get("refName").is_some_and(|r| !r.is_null()),
        changes: status.get("hasWorkingTreeChanges").and_then(Value::as_bool).unwrap_or(false),
        open_pr: status.pointer("/pr/state").and_then(Value::as_str) == Some("open"),
        upstream: status.get("hasUpstream").and_then(Value::as_bool).unwrap_or(false),
        remote: status.get("hasPrimaryRemote").and_then(Value::as_bool).unwrap_or(true),
        default_ref: status.get("isDefaultRef").and_then(Value::as_bool).unwrap_or(false),
        ahead,
        behind: num("behindCount"),
        ahead_of_default: status.get("aheadOfDefaultCount").and_then(Value::as_i64).unwrap_or(ahead),
    }
}

/// `resolveQuickAction`.
pub fn quick_action(status: Option<&Value>, busy: bool) -> QuickAction {
    let hint = |label: &str, hint: &str| QuickAction {
        label: label.into(),
        kind: QuickKind::Hint(hint.into()),
    };
    let run = |label: String, action: &'static str| QuickAction {
        label,
        kind: QuickKind::Run(action),
    };
    if busy {
        return hint("Commit", "Git action in progress.");
    }
    let Some(status) = status else {
        return hint("Commit", "Git status is unavailable.");
    };
    let pr = change_request_label(Some(status));
    let long = if pr == "MR" {
        "merge request"
    } else if pr == "PR" {
        "pull request"
    } else {
        "change request"
    };
    let f = facts(status);
    if !f.has_branch {
        return hint("Commit", &format!("Create and checkout a ref before pushing or opening a {long}."));
    }
    if f.changes {
        if !f.upstream && !f.remote {
            return run("Commit".into(), "commit");
        }
        if f.open_pr || f.default_ref {
            return run("Commit & push".into(), "commit_push");
        }
        return run(format!("Commit, push & {pr}"), "commit_push_pr");
    }
    let push = |f: &Facts| {
        if f.open_pr || f.default_ref {
            run("Push".into(), if f.default_ref { "commit_push" } else { "push" })
        } else {
            run(format!("Push & create {pr}"), "create_pr")
        }
    };
    if !f.upstream {
        if !f.remote {
            if f.open_pr && f.ahead == 0 {
                return QuickAction {
                    label: format!("View {pr}"),
                    kind: QuickKind::OpenPr,
                };
            }
            return QuickAction {
                label: "Publish repository".into(),
                kind: QuickKind::Publish,
            };
        }
        if f.ahead == 0 {
            if f.open_pr {
                return QuickAction {
                    label: format!("View {pr}"),
                    kind: QuickKind::OpenPr,
                };
            }
            return hint("Push", "No local commits to push.");
        }
        return push(&f);
    }
    if f.ahead > 0 && f.behind > 0 {
        return hint("Sync ref", "Branch has diverged from upstream. Rebase/merge first.");
    }
    if f.behind > 0 {
        return QuickAction {
            label: "Pull".into(),
            kind: QuickKind::Pull,
        };
    }
    if f.ahead > 0 {
        return push(&f);
    }
    if f.open_pr {
        return QuickAction {
            label: format!("View {pr}"),
            kind: QuickKind::OpenPr,
        };
    }
    if f.ahead_of_default > 0 && !f.default_ref {
        return run(format!("Create {pr}"), "create_pr");
    }
    hint("Commit", "Branch is up to date. No action needed.")
}

/// One entry of the git button's menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItem {
    /// `commit`, `push`, `pr`.
    pub id: &'static str,
    pub label: String,
    pub enabled: bool,
}

/// `buildMenuItems`: Commit, Push, then Create PR or View PR (only Commit without a remote).
pub fn menu_items(status: Option<&Value>, busy: bool) -> Vec<MenuItem> {
    let Some(status) = status else { return Vec::new() };
    let pr = change_request_label(Some(status));
    let f = facts(status);
    let push_without_upstream = f.remote && !f.upstream;
    let reachable = f.upstream || push_without_upstream;
    let commit = MenuItem {
        id: "commit",
        label: "Commit".into(),
        enabled: !busy && f.changes,
    };
    if !f.remote {
        return vec![commit];
    }
    vec![
        commit,
        MenuItem {
            id: "push",
            label: "Push".into(),
            enabled: !busy && f.has_branch && f.behind == 0 && f.ahead > 0 && reachable,
        },
        if f.open_pr {
            MenuItem {
                id: "pr",
                label: format!("View {pr}"),
                enabled: !busy,
            }
        } else {
            MenuItem {
                id: "pr",
                label: format!("Create {pr}"),
                enabled: !busy && f.has_branch && !f.changes && f.ahead_of_default > 0 && f.behind == 0 && reachable,
            }
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status(extra: Value) -> Value {
        let mut base = json!({
            "isRepo": true, "sourceControlProvider": {"kind": "github"}, "hasPrimaryRemote": true, "isDefaultRef": false,
            "refName": "feature", "hasWorkingTreeChanges": false, "hasUpstream": true, "aheadCount": 0, "behindCount": 0,
            "aheadOfDefaultCount": 0, "pr": null
        });
        for (k, v) in extra.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    #[test]
    fn the_quick_action_follows_the_web() {
        let label = |s: Value| quick_action(Some(&s), false).label;
        assert_eq!(label(status(json!({"hasWorkingTreeChanges": true}))), "Commit, push & PR");
        assert_eq!(label(status(json!({"hasWorkingTreeChanges": true, "isDefaultRef": true}))), "Commit & push");
        assert_eq!(label(status(json!({"aheadCount": 2}))), "Push & create PR");
        assert_eq!(label(status(json!({"behindCount": 1}))), "Pull");
        assert_eq!(label(status(json!({"aheadOfDefaultCount": 4, "pr": {"state": "merged"}}))), "Create PR");
        assert_eq!(label(status(json!({"pr": {"state": "open"}}))), "View PR");
        assert_eq!(
            label(status(json!({"sourceControlProvider": {"kind": "gitlab"}, "pr": {"state": "open"}}))),
            "View MR"
        );
        assert!(quick_action(Some(&status(json!({}))), false).disabled());
        assert!(quick_action(None, false).disabled());
        let menu = menu_items(Some(&status(json!({"aheadOfDefaultCount": 4}))), false);
        assert_eq!(
            menu.iter().map(|m| (m.id, m.enabled)).collect::<Vec<_>>(),
            [("commit", false), ("push", false), ("pr", true)]
        );
    }
}
