//! The page's numbers: what runs now, today, this week, per day and per project.

use std::collections::HashMap;

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};
use serde::Serialize;

use crate::model::{Agent, ProjectRef, Session};

/// How many days `perDay` covers, today included.
pub const DAYS: usize = 21;

const WEEK_MS: i64 = 7 * 86_400_000;

/// `GET /api/zenith/sessions`'s body.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Overview {
    /// When this was computed (ms since 1970); `live` is relative to it.
    pub generated_at: i64,
    /// The IANA zone days were counted in.
    pub time_zone: String,
    /// The most recent sessions, at most `limit`.
    pub sessions: Vec<Session>,
    pub totals: Totals,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    /// Every session found.
    pub sessions: usize,
    /// Wrote in the last 3 minutes.
    pub live: usize,
    /// Started today.
    pub today: usize,
    /// Every session's known cost (Claude Code's).
    #[serde(rename = "costUSD")]
    pub cost_usd: f64,
    /// Sessions active in the last 7 days (ended less than 7 days ago).
    pub week: WeekTotals,
    /// Sessions started each day, oldest first, [`DAYS`] days ending today.
    pub per_day: Vec<DayCount>,
    /// Every project with sessions, most sessions first; `project: null` is "no project".
    pub per_project: Vec<ProjectTotal>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WeekTotals {
    pub sessions: usize,
    #[serde(rename = "costUSD")]
    pub cost_usd: f64,
    pub tokens: i64,
    pub lines_added: i64,
    pub lines_removed: i64,
    pub prs: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct DayCount {
    /// `YYYY-MM-DD`.
    pub day: String,
    pub claude: usize,
    pub codex: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectTotal {
    pub project: Option<ProjectRef>,
    pub sessions: usize,
    #[serde(rename = "costUSD")]
    pub cost_usd: f64,
}

fn day_of(t: i64, tz: &TimeZone) -> Option<Date> {
    Some(Timestamp::from_millisecond(t).ok()?.to_zoned(tz.clone()).date())
}

/// The totals of `list` at `now_ms`, days counted in `tz`.
pub fn totals(list: &[Session], now_ms: i64, tz: &TimeZone) -> Totals {
    let today = day_of(now_ms, tz);
    let days: Vec<Date> = today
        .map(|d| (0..DAYS as i64).rev().filter_map(|n| d.checked_sub(n.days()).ok()).collect())
        .unwrap_or_default();
    let mut per_day: Vec<DayCount> = days
        .iter()
        .map(|d| DayCount {
            day: d.to_string(),
            claude: 0,
            codex: 0,
        })
        .collect();
    let index: HashMap<Date, usize> = days.iter().enumerate().map(|(i, d)| (*d, i)).collect();

    let mut t = Totals {
        sessions: list.len(),
        ..Totals::default()
    };
    let mut projects: Vec<ProjectTotal> = Vec::new();
    for s in list {
        let started = day_of(s.start, tz);
        t.live += usize::from(s.is_live_at(now_ms));
        t.today += usize::from(started.is_some() && started == today);
        t.cost_usd += s.cost_usd.unwrap_or(0.0);
        if s.end > now_ms - WEEK_MS {
            let w = &mut t.week;
            w.sessions += 1;
            w.cost_usd += s.cost_usd.unwrap_or(0.0);
            w.tokens += s.tokens.unwrap_or(0);
            w.lines_added += s.lines_added.unwrap_or(0);
            w.lines_removed += s.lines_removed.unwrap_or(0);
            w.prs += s.prs.len();
        }
        if let Some(&i) = started.as_ref().and_then(|d| index.get(d)) {
            match s.agent {
                Agent::Claude => per_day[i].claude += 1,
                Agent::Codex => per_day[i].codex += 1,
            }
        }
        let id = s.project.as_ref().map(|p| p.id.as_str());
        match projects.iter_mut().find(|p| p.project.as_ref().map(|p| p.id.as_str()) == id) {
            Some(p) => {
                p.sessions += 1;
                p.cost_usd += s.cost_usd.unwrap_or(0.0);
            }
            None => projects.push(ProjectTotal {
                project: s.project.clone(),
                sessions: 1,
                cost_usd: s.cost_usd.unwrap_or(0.0),
            }),
        }
    }
    // Most sessions first; "no project" last among equals.
    projects.sort_by(|a, b| b.sessions.cmp(&a.sessions).then(a.project.is_none().cmp(&b.project.is_none())));
    t.per_project = projects;
    t.per_day = per_day;
    t
}

/// The response: totals over `list`, and its first `limit` sessions.
pub fn overview(mut list: Vec<Session>, now_ms: i64, tz: &TimeZone, limit: usize) -> Overview {
    let totals = totals(&list, now_ms, tz);
    list.truncate(limit);
    Overview {
        generated_at: now_ms,
        time_zone: tz.iana_name().unwrap_or("UTC").to_owned(),
        sessions: list,
        totals,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Pr;
    use crate::parse::ms;

    fn session(agent: Agent, id: &str, start: &str, end: &str, project: Option<&str>, cost: Option<f64>) -> Session {
        Session {
            agent,
            id: id.into(),
            title: id.into(),
            project: project.map(|p| ProjectRef {
                id: p.into(),
                title: p.to_uppercase(),
            }),
            cwd: "/x".into(),
            branch: None,
            start: ms(start).unwrap(),
            end: ms(end).unwrap(),
            turns: 1,
            model: None,
            cost_usd: cost,
            tokens: Some(100),
            lines_added: Some(5),
            lines_removed: Some(1),
            prs: vec![Pr {
                number: 1,
                url: "https://example.test/pr/1".into(),
            }],
            subagents: 0,
            entrypoint: None,
            resume: String::new(),
            active_ms: 0,
            live: false,
            slots: vec![],
        }
    }

    #[test]
    fn counts_days_in_the_given_zone() {
        let tz = TimeZone::get("Europe/Paris").unwrap();
        // 23:30 UTC on Sep 30 is 01:30 on Oct 1 in Paris.
        let now = ms("2026-10-01T10:00:00Z").unwrap();
        let list = vec![
            session(Agent::Claude, "a", "2026-09-30T23:30:00Z", "2026-10-01T09:58:00Z", Some("acme"), Some(2.5)),
            session(Agent::Codex, "b", "2026-09-30T21:00:00Z", "2026-09-30T22:00:00Z", None, None),
            session(Agent::Claude, "c", "2026-09-01T10:00:00Z", "2026-09-02T10:00:00Z", Some("acme"), Some(1.0)),
        ];
        let t = totals(&list, now, &tz);
        assert_eq!((t.sessions, t.live, t.today), (3, 1, 1));
        assert_eq!(t.cost_usd, 3.5);
        assert_eq!(
            t.week,
            WeekTotals {
                sessions: 2,
                cost_usd: 2.5,
                tokens: 200,
                lines_added: 10,
                lines_removed: 2,
                prs: 2
            }
        );
        assert_eq!(t.per_day.len(), DAYS);
        let last = t.per_day.last().unwrap();
        assert_eq!((last.day.as_str(), last.claude, last.codex), ("2026-10-01", 1, 0));
        let before = &t.per_day[DAYS - 2];
        assert_eq!((before.day.as_str(), before.claude, before.codex), ("2026-09-30", 0, 1));
        assert_eq!(t.per_day[0].day, "2026-09-11");
        let per_project: Vec<_> = t
            .per_project
            .iter()
            .map(|p| (p.project.as_ref().map(|p| p.id.as_str()), p.sessions, p.cost_usd))
            .collect();
        assert_eq!(per_project, [(Some("acme"), 2, 3.5), (None, 1, 0.0)]);

        let o = overview(list, now, &tz, 1);
        assert_eq!((o.sessions.len(), o.totals.sessions, o.time_zone.as_str()), (1, 3, "Europe/Paris"));
        let json = serde_json::to_value(&o).unwrap();
        assert_eq!(json["totals"]["week"]["costUSD"], 2.5);
        assert_eq!(json["totals"]["perProject"][1]["project"], serde_json::Value::Null);
        assert_eq!(json["generatedAt"], now);
    }
}
