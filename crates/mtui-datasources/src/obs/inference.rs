//! Assignment/role inference — the plugin's exact state machine.
//!
//! Ported from upstream `mtui/data_sources/obs/inference.py`, which itself was
//! ported verbatim from openSUSE/osc-plugin-qam
//! (`oscqam/models/assignment.py` `Assignment.infer`/`infer_group`),
//! **GPL-2.0-only**. That GPL-2.0 provenance is preserved here per its
//! attribution requirement, independent of mtui's own license.
//!
//! This single source of truth backs BOTH `unassign`'s "the user holds >=1
//! assignment" guard and `approve`'s "the user is assigned" role check, so a
//! merely-*assigned*-but-not-*accepted* user does not count and a finished
//! reviewer is dropped — matching the plugin.
//!
//! The machine replays each qam group review's NESTED history in `when` order:
//! add an assignment on "Review got accepted", remove it on "Review got
//! reopened", ignore "Review got assigned"; then drop every assignment whose
//! user already has an accepted `by_user` review (a finished reviewer).

use std::collections::HashSet;

use chrono::{DateTime, NaiveDateTime};

use crate::obs::models::{Request, Review, is_qam_group};

const ASSIGNED: &str = "Review got assigned";
const ACCEPTED: &str = "Review got accepted";
const REOPENED: &str = "Review got reopened";

/// A resolved "`user` reviews for `group`" pairing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Assignment {
    /// The reviewing user.
    user: String,
    /// The qam group the user reviews for.
    pub(crate) group: String,
}

impl Assignment {
    fn new(user: impl Into<String>, group: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            group: group.into(),
        }
    }
}

/// A chronological sort key for a history `when` string.
///
/// Lenient, mirroring upstream `_when_key` (Python `datetime.fromisoformat`):
/// parses an offset-aware ISO-8601 timestamp (with or without a `Z`), falling
/// back to a naive `YYYY-MM-DDTHH:MM:SS`; both normalise to a naive UTC instant
/// so all keys compare. Unparseable values sort **last** (bucket 1), matching
/// Python's `(1, datetime.max)`.
fn when_key(when: &str) -> (u8, Option<NaiveDateTime>) {
    let trimmed = when.trim().replace('Z', "+00:00");
    if let Ok(dt) = DateTime::parse_from_rfc3339(&trimmed) {
        return (0, Some(dt.naive_utc()));
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(when.trim(), "%Y-%m-%dT%H:%M:%S") {
        return (0, Some(dt));
    }
    (1, None)
}

/// Replay one group review's relevant history into the assignments it implies.
///
/// A stable sort by [`when_key`] preserves document order for equal instants and
/// for the unparseable bucket, matching Python's stable `sorted`.
fn infer_group(review: &Review, group: &str) -> HashSet<Assignment> {
    let mut events: Vec<&crate::obs::models::HistoryEvent> = review
        .history
        .iter()
        .filter(|e| matches!(e.description.as_str(), ASSIGNED | ACCEPTED | REOPENED))
        .collect();
    events.sort_by_key(|e| when_key(&e.when));

    let mut assignments: HashSet<Assignment> = HashSet::new();
    for event in events {
        match event.description.as_str() {
            ACCEPTED => {
                assignments.insert(Assignment::new(&event.who, group));
            }
            REOPENED => {
                assignments.remove(&Assignment::new(&event.who, group));
            }
            // ASSIGNED is a no-op (a group review being picked up is not yet a
            // completed assignment).
            _ => {}
        }
    }
    assignments
}

/// Resolve the full set of active user->group assignments for a request.
#[must_use]
fn infer(request: &Request) -> HashSet<Assignment> {
    let mut assignments: HashSet<Assignment> = HashSet::new();
    for review in &request.reviews {
        if let Some(group) = review.by_group.as_deref()
            && is_qam_group(group)
            && matches!(review.state.as_str(), "accepted" | "new")
        {
            assignments.extend(infer_group(review, group));
        }
    }

    let finished_users: HashSet<&str> = request
        .reviews
        .iter()
        .filter(|r| r.state == "accepted")
        .filter_map(|r| r.by_user.as_deref())
        .collect();

    assignments
        .into_iter()
        .filter(|a| !finished_users.contains(a.user.as_str()))
        .collect()
}

/// The subset of [`infer`] assignments belonging to `user`.
#[must_use]
pub(crate) fn assignments_for_user(request: &Request, user: &str) -> HashSet<Assignment> {
    infer(request)
        .into_iter()
        .filter(|a| a.user == user)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::models::parse_request;

    const ACCEPT: &str = "Review got accepted";
    const ASSIGN: &str = "Review got assigned";
    const REOPEN: &str = "Review got reopened";

    /// Build a `<request>` document wrapping the given review fragments.
    fn request(reviews: &[String]) -> String {
        format!(
            "<request id='1'><state name='review'/>{}</request>",
            reviews.concat()
        )
    }

    /// Build a `<review by_group=…>` with the given `(who, when, desc)` history.
    fn group_review(group: &str, state: &str, events: &[(&str, &str, &str)]) -> String {
        let history: String = events
            .iter()
            .map(|(who, when, desc)| {
                format!(
                    "<history who='{who}' when='{when}'><description>{desc}</description></history>"
                )
            })
            .collect();
        format!("<review state='{state}' by_group='{group}'>{history}</review>")
    }

    fn assignment(user: &str, group: &str) -> Assignment {
        Assignment::new(user, group)
    }

    #[test]
    fn accepted_history_yields_assignment() {
        let req = parse_request(&request(&[group_review(
            "qam-sle",
            "accepted",
            &[("alice", "2017-01-01T00:00:00", ACCEPT)],
        )]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::from([assignment("alice", "qam-sle")]));
    }

    #[test]
    fn assigned_only_does_not_count() {
        let req = parse_request(&request(&[group_review(
            "qam-sle",
            "new",
            &[("alice", "2017-01-01T00:00:00", ASSIGN)],
        )]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::new());
    }

    #[test]
    fn reopened_after_accepted_removes_assignment() {
        let req = parse_request(&request(&[group_review(
            "qam-sle",
            "new",
            &[
                ("alice", "2017-01-01T00:00:00", ACCEPT),
                ("alice", "2017-01-02T00:00:00", REOPEN),
            ],
        )]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::new());
    }

    #[test]
    fn out_of_order_history_is_sorted_by_when() {
        // Events replay in `when` order regardless of document order.
        let req = parse_request(&request(&[group_review(
            "qam-sle",
            "new",
            &[
                ("alice", "2017-01-02T00:00:00", REOPEN), // later, listed first
                ("alice", "2017-01-01T00:00:00", ACCEPT), // earlier
            ],
        )]))
        .unwrap();
        // Chronologically: accepted then reopened -> no assignment.
        assert_eq!(infer(&req), HashSet::new());
    }

    #[test]
    fn finished_user_review_drops_assignment() {
        let req = parse_request(&request(&[
            group_review(
                "qam-sle",
                "accepted",
                &[("alice", "2017-01-01T00:00:00", ACCEPT)],
            ),
            "<review state='accepted' by_user='alice'/>".to_owned(),
        ]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::new());
    }

    #[test]
    fn automation_groups_are_ignored() {
        let req = parse_request(&request(&[
            group_review(
                "qam-auto",
                "accepted",
                &[("bot", "2017-01-01T00:00:00", ACCEPT)],
            ),
            group_review(
                "qam-openqa",
                "accepted",
                &[("bot", "2017-01-01T00:00:00", ACCEPT)],
            ),
        ]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::new());
    }

    #[test]
    fn assignments_for_user_filters() {
        let req = parse_request(&request(&[
            group_review(
                "qam-sle",
                "accepted",
                &[("alice", "2017-01-01T00:00:00", ACCEPT)],
            ),
            group_review(
                "qam-cloud",
                "accepted",
                &[("bob", "2017-01-01T00:00:00", ACCEPT)],
            ),
        ]))
        .unwrap();
        assert_eq!(
            assignments_for_user(&req, "alice"),
            HashSet::from([assignment("alice", "qam-sle")])
        );
    }

    #[test]
    fn timezone_aware_when_is_normalised() {
        // A `Z`/offset timestamp is parsed and ordered against naive ones.
        let req = parse_request(&request(&[group_review(
            "qam-sle",
            "new",
            &[
                ("alice", "2017-01-01T00:00:00Z", ACCEPT),
                ("alice", "2017-01-02T00:00:00+00:00", REOPEN),
            ],
        )]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::new());
    }

    #[test]
    fn unparseable_when_sorts_last_without_crashing() {
        let req = parse_request(&request(&[group_review(
            "qam-sle",
            "new",
            &[
                ("alice", "not-a-date", ACCEPT),
                ("alice", "2017-01-01T00:00:00", ACCEPT),
            ],
        )]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::from([assignment("alice", "qam-sle")]));
    }
}
