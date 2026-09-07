//! Assignment/role inference — which user reviews for which qam group.
//!
//! Derived from openSUSE/osc-plugin-qam
//! (`oscqam/models/assignment.py` `Assignment.infer`/`infer_group`),
//! **GPL-2.0-only**, with mtui's own reopen rule (#596). That GPL-2.0
//! provenance is preserved here per its attribution requirement, independent
//! of mtui's own license.
//!
//! This single source of truth backs BOTH `unassign`'s "the user holds >=1
//! assignment" guard (and the groups it reverts) and `approve`'s "the user is
//! assigned" check.
//!
//! OBS has no assignment concept; `assignreview` leaves two traces in one
//! transaction: an open (`new`) `by_user` review for the assignee — created
//! with a "Review got assigned" event, or their existing one flipped back to
//! `new` — and an event on the qam group review whose `who` is the API caller
//! (the assignee, since mtui self-assigns): "Review got accepted" on a `new`
//! group review, "Review got reopened" on one a prior tester already accepted,
//! where the state stays `accepted`. `assignreview revert=1` also logs
//! "Review got reopened" but destroys the user review, so a reopen is an
//! assignment only when it is not older than the actor's open user review:
//! that review is written before the group event, and no review survives a
//! revert.
//!
//! The machine replays each qam group review's NESTED history in `when` order:
//! "accepted" adds (who, group); "reopened" adds (who, group) when `who` holds
//! an open user review whose "Review got assigned" is not later than the
//! reopen, and removes it otherwise; "assigned" is ignored. An assignment is
//! then finished — dropped — when its user approved (a "Review got accepted"
//! on any of their user reviews) at or after the event that produced it; an
//! accepted user review without such an event finishes all of them.

use std::collections::{HashMap, HashSet};

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

/// The instant a history `when` string names, when it names one.
///
/// Lenient, akin to Python's `datetime.fromisoformat`: an offset-aware
/// ISO-8601 timestamp (with or without a `Z`), falling back to a naive
/// `YYYY-MM-DDTHH:MM:SS`; both normalise to a naive UTC instant so all
/// instants compare.
fn instant(when: &str) -> Option<NaiveDateTime> {
    let trimmed = when.trim().replace('Z', "+00:00");
    if let Ok(dt) = DateTime::parse_from_rfc3339(&trimmed) {
        return Some(dt.naive_utc());
    }
    NaiveDateTime::parse_from_str(when.trim(), "%Y-%m-%dT%H:%M:%S").ok()
}

/// A chronological sort key for a history `when` string: unparseable values
/// sort **last** (bucket 1).
///
/// Ordering only. Never compare two of these to place events against each
/// other in time — the unparseable bucket would read as "later".
fn when_key(when: &str) -> (u8, Option<NaiveDateTime>) {
    let at = instant(when);
    (u8::from(at.is_none()), at)
}

/// The `by_user` reviews of a request, reduced to what the rules need.
struct UserReviews<'a> {
    /// Per user, the latest placeable "Review got assigned" on an open review.
    assigned_at: HashMap<&'a str, NaiveDateTime>,
    /// Per user, one entry per approval; `None` where it cannot be placed.
    approvals: HashMap<&'a str, Vec<Option<NaiveDateTime>>>,
}

impl<'a> UserReviews<'a> {
    fn collect(request: &'a Request) -> Self {
        let mut assigned_at: HashMap<&'a str, NaiveDateTime> = HashMap::new();
        let mut approvals: HashMap<&'a str, Vec<Option<NaiveDateTime>>> = HashMap::new();
        for review in &request.reviews {
            let Some(user) = review.by_user.as_deref() else {
                continue;
            };
            if review.state == "new" {
                for at in review
                    .history
                    .iter()
                    .filter(|e| e.description == ASSIGNED)
                    .filter_map(|e| instant(&e.when))
                {
                    assigned_at
                        .entry(user)
                        .and_modify(|latest| *latest = (*latest).max(at))
                        .or_insert(at);
                }
            }
            // Any state: `assignreview` reuses a user review, flipping it back
            // to `new` while its history keeps the earlier approval.
            let mut accepted = review
                .history
                .iter()
                .filter(|e| e.description == ACCEPTED)
                .map(|e| instant(&e.when))
                .peekable();
            if accepted.peek().is_some() {
                approvals.entry(user).or_default().extend(accepted);
            } else if review.state == "accepted" {
                // An accepted review OBS served no accept event for is still
                // an approval, just one with no instant.
                approvals.entry(user).or_default().push(None);
            }
        }
        Self {
            assigned_at,
            approvals,
        }
    }

    /// Whether `user`'s open user review carries a "Review got assigned" no
    /// later than `at`.
    ///
    /// False when either instant is missing: an event that cannot be placed
    /// cannot be shown to follow the assignment record, and refusing one only
    /// reads as "not assigned", where a phantom is a live `assignreview
    /// revert=1` against a group the user does not hold.
    fn assigned_before(&self, user: &str, at: Option<NaiveDateTime>) -> bool {
        match (self.assigned_at.get(user), at) {
            (Some(assigned), Some(at)) => at >= *assigned,
            _ => false,
        }
    }

    /// Whether `user` approved at or after an assignment produced at `at`.
    ///
    /// A user with no approval finishes nothing. Otherwise an approval or an
    /// assignment that cannot be placed in time finishes it, for the reason
    /// [`Self::assigned_before`] refuses.
    fn finished(&self, user: &str, at: Option<NaiveDateTime>) -> bool {
        self.approvals.get(user).is_some_and(|approvals| {
            approvals.iter().any(|approval| match (approval, at) {
                (Some(approval), Some(at)) => *approval >= at,
                _ => true,
            })
        })
    }
}

/// Replay one group review's relevant history into the assignments it implies,
/// each with the instant of the event that produced it (`None`: unparseable).
///
/// A "reopened" is an assignment only when its actor's open user review has a
/// "Review got assigned" not later than it. A reassignment onto an
/// already-accepted group review is logged as a reopen by the new assignee
/// after their user review was written; a revert destroys the user review, so
/// a reopen older than the review the actor holds now cannot be told apart
/// from a revert — on this group or, since a user review names no group, on
/// any other — and is treated as one. Every other reopen is an un-assignment,
/// as the plugin treated all of them.
///
/// A stable sort by [`when_key`] preserves document order for equal instants and
/// for the unparseable bucket.
fn infer_group(
    review: &Review,
    group: &str,
    users: &UserReviews<'_>,
) -> HashMap<Assignment, Option<NaiveDateTime>> {
    let mut events: Vec<&crate::obs::models::HistoryEvent> = review
        .history
        .iter()
        .filter(|e| matches!(e.description.as_str(), ASSIGNED | ACCEPTED | REOPENED))
        .collect();
    events.sort_by_key(|e| when_key(&e.when));

    let mut assignments: HashMap<Assignment, Option<NaiveDateTime>> = HashMap::new();
    for event in events {
        let at = instant(&event.when);
        match event.description.as_str() {
            ACCEPTED => {
                assignments.insert(Assignment::new(&event.who, group), at);
            }
            REOPENED if users.assigned_before(&event.who, at) => {
                assignments.insert(Assignment::new(&event.who, group), at);
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

/// Resolve the active user->group assignments for a request: every qam group
/// review replayed, minus the ones their user approved at or after the
/// producing event.
#[must_use]
fn infer(request: &Request) -> HashSet<Assignment> {
    let users = UserReviews::collect(request);

    let mut assignments: HashMap<Assignment, Option<NaiveDateTime>> = HashMap::new();
    for review in &request.reviews {
        if let Some(group) = review.by_group.as_deref()
            && is_qam_group(group)
            && matches!(review.state.as_str(), "accepted" | "new")
        {
            assignments.extend(infer_group(review, group, &users));
        }
    }

    assignments
        .into_iter()
        .filter(|(assignment, at)| !users.finished(&assignment.user, *at))
        .map(|(assignment, _)| assignment)
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

    fn history(events: &[(&str, &str, &str)]) -> String {
        events
            .iter()
            .map(|(who, when, desc)| {
                format!(
                    "<history who='{who}' when='{when}'><description>{desc}</description></history>"
                )
            })
            .collect()
    }

    /// Build a `<review by_group=…>` with the given `(who, when, desc)` history.
    fn group_review(group: &str, state: &str, events: &[(&str, &str, &str)]) -> String {
        format!(
            "<review state='{state}' by_group='{group}'>{}</review>",
            history(events)
        )
    }

    /// Build a `<review by_user=…>` with the given `(who, when, desc)` history.
    fn user_review(user: &str, state: &str, events: &[(&str, &str, &str)]) -> String {
        format!(
            "<review state='{state}' by_user='{user}'>{}</review>",
            history(events)
        )
    }

    // Timestamps of the request in the issue (users anonymised).
    const T_PRIOR_ASSIGN: &str = "2026-09-01T00:33:57";
    const T_PRIOR_APPROVE: &str = "2026-09-01T07:01:36";
    const T_REASSIGN: &str = "2026-09-06T08:58:47";

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

    /// An approval that cannot be placed in time finishes every assignment its
    /// user holds: refusing one is a harmless "not assigned", a phantom is a
    /// live revert against a group they do not hold. The review is `new`, so
    /// only the approval event can finish her.
    #[test]
    fn approval_with_unparseable_when_finishes_every_assignment() {
        let req = parse_request(&request(&[
            group_review("qam-sle", "accepted", &[("alice", T_PRIOR_ASSIGN, ACCEPT)]),
            user_review(
                "alice",
                "new",
                &[
                    ("alice", T_PRIOR_ASSIGN, ASSIGN),
                    ("alice", "not-a-date", ACCEPT),
                ],
            ),
        ]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::new());
    }

    /// An assignment whose own event cannot be placed in time is finished by
    /// any approval its user made.
    #[test]
    fn assignment_with_unparseable_when_is_finished_by_any_approval() {
        let req = parse_request(&request(&[
            group_review("qam-sle", "accepted", &[("alice", "not-a-date", ACCEPT)]),
            user_review("alice", "accepted", &[("alice", T_PRIOR_APPROVE, ACCEPT)]),
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

    /// #596: the request as OBS served it.
    #[test]
    fn reassignment_after_prior_tester_approved_yields_new_assignee() {
        let req = parse_request(&request(&[
            group_review(
                "qam-sle",
                "accepted",
                &[
                    ("bob", T_PRIOR_ASSIGN, ACCEPT),
                    ("alice", T_REASSIGN, REOPEN),
                ],
            ),
            user_review(
                "bob",
                "accepted",
                &[
                    ("bob", T_PRIOR_ASSIGN, ASSIGN),
                    ("bob", T_PRIOR_APPROVE, ACCEPT),
                ],
            ),
            user_review("alice", "new", &[("alice", T_REASSIGN, ASSIGN)]),
        ]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::from([assignment("alice", "qam-sle")]));
    }

    /// A "reopened" by someone without an open user review (an admin
    /// reopening the group review) assigns nobody.
    #[test]
    fn reopened_by_actor_without_open_review_does_not_assign() {
        let req = parse_request(&request(&[
            group_review(
                "qam-sle",
                "new",
                &[
                    ("bob", T_PRIOR_ASSIGN, ACCEPT),
                    ("carol", T_REASSIGN, REOPEN),
                ],
            ),
            user_review("bob", "new", &[("bob", T_PRIOR_ASSIGN, ASSIGN)]),
        ]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::from([assignment("bob", "qam-sle")]));
    }

    /// A self-revert before another tester took the group: alice's "reopened"
    /// predates the user review she holds now, so it stays a removal.
    #[test]
    fn reopened_before_a_later_acceptance_is_not_an_assignment() {
        let req = parse_request(&request(&[
            group_review(
                "qam-sle",
                "accepted",
                &[
                    ("alice", T_PRIOR_ASSIGN, ACCEPT),
                    ("alice", "2026-09-02T00:00:00", REOPEN),
                    ("bob", T_REASSIGN, ACCEPT),
                ],
            ),
            user_review("alice", "new", &[("alice", T_REASSIGN, ASSIGN)]),
            user_review("bob", "new", &[("bob", T_REASSIGN, ASSIGN)]),
        ]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::from([assignment("bob", "qam-sle")]));
    }

    /// Maintainer probe: `open_users` is request-scoped, so a stale self-revert
    /// on one group must not read as an assignment once its actor is assigned
    /// to another. alice took qam-sle, gave it back, then took qam-manager.
    #[test]
    fn probe_stale_self_revert_on_another_group() {
        let req = parse_request(&request(&[
            group_review(
                "qam-sle",
                "new",
                &[
                    ("alice", "2026-09-01T00:00:00", ACCEPT),
                    ("alice", "2026-09-02T00:00:00", REOPEN),
                ],
            ),
            group_review(
                "qam-manager",
                "new",
                &[("alice", "2026-09-03T00:00:00", ACCEPT)],
            ),
            user_review("alice", "new", &[("alice", "2026-09-03T00:00:00", ASSIGN)]),
        ]))
        .unwrap();
        assert_eq!(
            infer(&req),
            HashSet::from([assignment("alice", "qam-manager")])
        );
    }

    /// A "reopened" whose `when` does not parse cannot be placed against the
    /// actor's assignment record, so it stays a removal.
    #[test]
    fn reopen_with_unparseable_when_is_a_removal() {
        let req = parse_request(&request(&[
            group_review(
                "qam-sle",
                "accepted",
                &[
                    ("bob", T_PRIOR_ASSIGN, ACCEPT),
                    ("alice", "not-a-date", REOPEN),
                ],
            ),
            user_review(
                "bob",
                "accepted",
                &[
                    ("bob", T_PRIOR_ASSIGN, ASSIGN),
                    ("bob", T_PRIOR_APPROVE, ACCEPT),
                ],
            ),
            user_review("alice", "new", &[("alice", T_REASSIGN, ASSIGN)]),
        ]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::new());
    }

    /// An assignment record whose `when` does not parse places nothing in
    /// time, so its holder's "reopened" events stay removals.
    #[test]
    fn assignment_record_with_unparseable_when_refuses_reopens() {
        let req = parse_request(&request(&[
            group_review(
                "qam-sle",
                "accepted",
                &[
                    ("bob", T_PRIOR_ASSIGN, ACCEPT),
                    ("alice", T_REASSIGN, REOPEN),
                ],
            ),
            user_review(
                "bob",
                "accepted",
                &[
                    ("bob", T_PRIOR_ASSIGN, ASSIGN),
                    ("bob", T_PRIOR_APPROVE, ACCEPT),
                ],
            ),
            user_review("alice", "new", &[("alice", "not-a-date", ASSIGN)]),
        ]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::new());
    }

    /// `assignreview` always writes a "Review got assigned", so an open user
    /// review carrying none is no assignment record and a "reopened" cannot
    /// lean on the review's mere existence.
    #[test]
    fn open_review_without_an_assignment_record_refuses_reopens() {
        let req = parse_request(&request(&[
            group_review(
                "qam-sle",
                "accepted",
                &[
                    ("bob", T_PRIOR_ASSIGN, ACCEPT),
                    ("alice", T_REASSIGN, REOPEN),
                ],
            ),
            user_review(
                "bob",
                "accepted",
                &[
                    ("bob", T_PRIOR_ASSIGN, ASSIGN),
                    ("bob", T_PRIOR_APPROVE, ACCEPT),
                ],
            ),
            user_review("alice", "new", &[]),
        ]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::new());
    }

    /// Maintainer probe: alice approved qam-sle, then was assigned qam-manager
    /// (a second `by_user` review). Her approval finishes only the assignment
    /// it followed. It landed within the second that produced that assignment
    /// — OBS timestamps have second resolution — so the comparison is `>=`.
    #[test]
    fn probe_finished_group_a_plus_fresh_group_b() {
        let req = parse_request(&request(&[
            group_review(
                "qam-sle",
                "accepted",
                &[("alice", "2026-09-01T00:00:00", ACCEPT)],
            ),
            group_review(
                "qam-manager",
                "new",
                &[("alice", "2026-09-03T00:00:00", ACCEPT)],
            ),
            user_review(
                "alice",
                "accepted",
                &[("alice", "2026-09-01T00:00:00", ACCEPT)],
            ),
            user_review("alice", "new", &[("alice", "2026-09-03T00:00:00", ASSIGN)]),
        ]))
        .unwrap();
        assert_eq!(
            infer(&req),
            HashSet::from([assignment("alice", "qam-manager")])
        );
    }

    /// The shape `assignreview` actually writes: it reuses alice's `by_user`
    /// review, flipping it back to `new` while its history keeps the earlier
    /// "Review got accepted". Her approval finishes qam-sle alone; bob's
    /// finishes the group he handed over.
    #[test]
    fn approved_then_reassigned_on_a_reused_user_review() {
        let req = parse_request(&request(&[
            group_review("qam-sle", "accepted", &[("alice", T_PRIOR_ASSIGN, ACCEPT)]),
            group_review(
                "qam-manager",
                "accepted",
                &[
                    ("bob", "2026-08-31T09:00:00", ACCEPT),
                    ("alice", T_REASSIGN, REOPEN),
                ],
            ),
            user_review(
                "bob",
                "accepted",
                &[
                    ("bob", "2026-08-31T09:00:00", ASSIGN),
                    ("bob", "2026-08-31T15:00:00", ACCEPT),
                ],
            ),
            user_review(
                "alice",
                "new",
                &[
                    ("alice", T_PRIOR_ASSIGN, ASSIGN),
                    ("alice", T_PRIOR_APPROVE, ACCEPT),
                    ("alice", T_REASSIGN, REOPEN),
                ],
            ),
        ]))
        .unwrap();
        assert_eq!(
            infer(&req),
            HashSet::from([assignment("alice", "qam-manager")])
        );
    }

    /// Re-assigned, then self-unassigned: the revert destroyed the user
    /// review, so neither of alice's "reopened" events assigns her.
    #[test]
    fn reassigned_then_self_unassigned_is_removed() {
        let req = parse_request(&request(&[
            group_review(
                "qam-sle",
                "new",
                &[
                    ("bob", T_PRIOR_ASSIGN, ACCEPT),
                    ("alice", T_REASSIGN, REOPEN),
                    ("alice", "2026-09-06T09:00:00", REOPEN),
                ],
            ),
            user_review(
                "bob",
                "accepted",
                &[
                    ("bob", T_PRIOR_ASSIGN, ASSIGN),
                    ("bob", T_PRIOR_APPROVE, ACCEPT),
                ],
            ),
        ]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::new());
    }

    /// A departed tester's "reopened" (their own revert) must stay a removal
    /// once someone else is assigned.
    #[test]
    fn unassigned_then_reassigned_yields_only_the_new_assignee() {
        let req = parse_request(&request(&[
            group_review(
                "qam-sle",
                "accepted",
                &[
                    ("alice", T_PRIOR_ASSIGN, ACCEPT),
                    ("alice", "2026-09-02T00:00:00", REOPEN),
                    ("bob", T_REASSIGN, ACCEPT),
                ],
            ),
            user_review("bob", "new", &[("bob", T_REASSIGN, ASSIGN)]),
        ]))
        .unwrap();
        assert_eq!(infer(&req), HashSet::from([assignment("bob", "qam-sle")]));
    }

    /// Only an open (`new`) user review makes a "reopened" an assignment; a
    /// declined one does not.
    #[test]
    fn reopened_with_only_a_declined_user_review_does_not_assign() {
        let req = parse_request(&request(&[
            group_review(
                "qam-sle",
                "new",
                &[
                    ("alice", T_PRIOR_ASSIGN, ACCEPT),
                    ("alice", T_REASSIGN, REOPEN),
                ],
            ),
            user_review("alice", "declined", &[("alice", T_PRIOR_ASSIGN, ASSIGN)]),
        ]))
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
