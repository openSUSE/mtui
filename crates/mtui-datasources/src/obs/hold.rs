//! Who holds a qam group's review on an OBS request, read off the reviews
//! `assignreview` leaves behind — `assign`'s pre-POST check (#599).
//! [`inference`](super::inference) answers the same question for
//! `unassign`/`approve` from the group review's history; both read a `new`
//! `by_user` review as "held". They differ where the document is ambiguous:
//! a group accepted with no user review is approved here but assigned there,
//! and a proxy assign (`reviewer=X` posted by W) is held by X here — the
//! comment names X and the review is X's — and by W there; both refuse, so X's
//! `assign` refusal points at an `unassign` that refuses too.
//!
//! OBS exposes the group↔user link only as the `by_user` review's comment,
//! `reassigned review for group {G} to user {U}`
//! (`BsRequest#reassign_review_comment`), rewritten on every assign, so a user
//! assigned to two groups names only the last one. When no open user review's
//! comment names this group, the group review's last accepted/reopened actor
//! stands in — a guess, so `HeldBy` states only what the document holds: the
//! group review is accepted and that user has an open review. An approver is
//! named from the group review's own `Review got accepted` event: a `by_user`
//! review's `when` is that user's, not this group's.

use chrono::NaiveDateTime;

use crate::obs::inference::instant;
use crate::obs::models::{HistoryEvent, Request, Review};

const ACCEPTED: &str = "Review got accepted";
const REOPENED: &str = "Review got reopened";

/// One group's review as `assign` must see it before posting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GroupHold {
    /// No live (`new`/`accepted`) review for the group, or an open group review.
    Free,
    /// Accepted with no open user review the document ties to the group; `by`
    /// = the group review's own `Review got accepted` event, when it has one.
    Approved { by: Option<(String, String)> },
    /// The caller holds the open user review for the group.
    HeldByMe,
    /// Another user has an open review the document ties to the group — by its
    /// comment, or by the group review's last actor.
    HeldBy { user: String },
}

/// How `group`'s review on `request` stands for `user`.
pub(crate) fn group_hold(request: &Request, group: &str, user: &str) -> GroupHold {
    // Same liveness filter as `inference`: superseded/obsoleted/declined
    // entries are invisible.
    let live: Vec<&Review> = request
        .reviews
        .iter()
        .filter(|r| r.by_group.as_deref() == Some(group))
        .filter(|r| matches!(r.state.as_str(), "new" | "accepted"))
        .collect();
    // `assignreview` always leaves the group review `accepted`, so an open one
    // is nobody's.
    if live.iter().any(|r| r.state == "new") {
        return GroupHold::Free;
    }
    // Reviews serialise in creation order, so the last live one is the current
    // cycle's and an earlier accepted one closed a superseded cycle. Not by
    // `when`: OBS omits it on the group reviews it serves, unlike a history
    // event's, which is written when the event is.
    let Some(group_review) = live.last().copied() else {
        return GroupHold::Free;
    };
    let last_actor = last_event(group_review, &[ACCEPTED, REOPENED]).map(|e| e.who.as_str());
    let user_reviews = || {
        request
            .reviews
            .iter()
            .filter(|r| r.state == "new" && r.by_user.is_some())
    };
    // OBS's own group<->user link, where it survives.
    let linked: Vec<&Review> = user_reviews()
        .filter(|r| {
            reassigned_group(&r.comment)
                .is_some_and(|(g, u)| g == group && Some(u) == r.by_user.as_deref())
        })
        .collect();
    // Nothing names this group: the group review's last actor is the only
    // evidence left, and it is a guess — see the message below.
    let open: Vec<&Review> = if linked.is_empty() {
        user_reviews()
            .filter(|r| r.by_user.as_deref() == last_actor)
            .collect()
    } else {
        linked
    };
    if open.iter().any(|r| r.by_user.as_deref() == Some(user)) {
        return GroupHold::HeldByMe;
    }
    if let Some(holder) = open
        .iter()
        .find(|r| r.by_user.as_deref() == last_actor)
        .or_else(|| open.iter().max_by_key(|r| latest_key(&r.when)))
    {
        return GroupHold::HeldBy {
            user: holder.by_user.clone().unwrap_or_default(),
        };
    }
    let by = last_event(group_review, &[ACCEPTED]).map(|e| (e.who.clone(), e.when.clone()));
    GroupHold::Approved { by }
}

/// The latest of `review`'s events with one of `descriptions`; the last in
/// document order on a tie or an unparseable `when`.
fn last_event<'a>(review: &'a Review, descriptions: &[&str]) -> Option<&'a HistoryEvent> {
    review
        .history
        .iter()
        .filter(|e| descriptions.contains(&e.description.as_str()))
        .max_by_key(|e| latest_key(&e.when))
}

/// Sort key for picking the single latest of several: a parseable timestamp
/// outranks an unparseable one, so `max_by_key` never returns an unplaceable
/// entry while a placeable one exists. Opposite tie-break to
/// `inference::when_key`, which orders a whole history for replay and parks
/// unparseable entries last.
fn latest_key(when: &str) -> (bool, Option<NaiveDateTime>) {
    let at = instant(when);
    (at.is_some(), at)
}

/// `reassigned review for group {G} to user {U}` → `(G, U)`; anything else
/// (incl. the group review's `review reassigned to user {U}`) → `None`.
pub(crate) fn reassigned_group(comment: &str) -> Option<(&str, &str)> {
    let rest = comment
        .trim()
        .strip_prefix("reassigned review for group ")?;
    let (group, user) = rest.split_once(" to user ")?;
    (!group.is_empty() && !user.is_empty()).then_some((group, user))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::models::parse_request;

    const T_ASSIGNED: &str = "2026-09-01T00:33:57";
    const T_APPROVED: &str = "2026-09-02T07:25:47";
    const T_REASSIGNED: &str = "2026-09-06T11:24:48";

    fn req(reviews: &str) -> Request {
        parse_request(&format!(
            "<request id='1'><state name='review'/>{reviews}</request>"
        ))
        .unwrap()
    }

    fn grp(group: &str, state: &str, events: &[(&str, &str, &str)]) -> String {
        let hist: String = events
            .iter()
            .map(|(w, t, d)| {
                format!("<history who='{w}' when='{t}'><description>{d}</description></history>")
            })
            .collect();
        format!("<review state='{state}' by_group='{group}'>{hist}</review>")
    }

    fn usr(user: &str, state: &str, when: &str, comment: &str) -> String {
        format!(
            "<review state='{state}' when='{when}' who='{user}' by_user='{user}'>\
             <comment>{comment}</comment></review>"
        )
    }

    fn reassigned(group: &str, user: &str) -> String {
        format!("reassigned review for group {group} to user {user}")
    }

    fn held_by(user: &str) -> GroupHold {
        GroupHold::HeldBy {
            user: user.to_owned(),
        }
    }

    #[test]
    fn reassigned_group_parses_only_the_user_review_comment() {
        assert_eq!(
            reassigned_group("reassigned review for group qam-sle to user alice"),
            Some(("qam-sle", "alice"))
        );
        assert_eq!(reassigned_group("review reassigned to user alice"), None);
        assert_eq!(reassigned_group(""), None);
        assert_eq!(
            reassigned_group("reassigned review for group  to user alice"),
            None
        );
    }

    #[test]
    fn no_live_group_review_is_free() {
        assert_eq!(group_hold(&req(""), "qam-sle", "alice"), GroupHold::Free);
        let superseded = grp("qam-sle", "superseded", &[("bob", T_ASSIGNED, ACCEPTED)]);
        assert_eq!(
            group_hold(&req(&superseded), "qam-sle", "alice"),
            GroupHold::Free
        );
    }

    #[test]
    fn open_group_review_is_free() {
        let open = "<review state='new' by_group='qam-sle'/>";
        assert_eq!(group_hold(&req(open), "qam-sle", "alice"), GroupHold::Free);
        let re_requested = grp("qam-sle", "accepted", &[("alice", T_ASSIGNED, ACCEPTED)]) + open;
        assert_eq!(
            group_hold(&req(&re_requested), "qam-sle", "bob"),
            GroupHold::Free
        );
    }

    #[test]
    fn open_user_review_naming_the_group_is_held() {
        let reviews = grp("qam-sle", "accepted", &[("bob", T_ASSIGNED, ACCEPTED)])
            + &usr("bob", "new", T_ASSIGNED, &reassigned("qam-sle", "bob"));
        let request = req(&reviews);
        assert_eq!(group_hold(&request, "qam-sle", "alice"), held_by("bob"));
        assert_eq!(group_hold(&request, "qam-sle", "bob"), GroupHold::HeldByMe);
    }

    /// One tester, two groups: the rewritten comment names qam-cloud alone, so
    /// qam-sle falls back to its own last actor. Encodes which of the two
    /// readings this document allows the code picks — "alice holds both" over
    /// "alice approved qam-sle" — because a wrong `HeldBy` is forceable and its
    /// remedy is `unassign`, while a wrong `Approved` is neither.
    #[test]
    fn a_tester_assigned_to_two_groups_holds_both() {
        let reviews = grp("qam-sle", "accepted", &[("alice", T_ASSIGNED, ACCEPTED)])
            + &grp("qam-cloud", "accepted", &[("alice", T_APPROVED, ACCEPTED)])
            + &usr(
                "alice",
                "new",
                T_APPROVED,
                &reassigned("qam-cloud", "alice"),
            );
        let request = req(&reviews);
        assert_eq!(group_hold(&request, "qam-sle", "bob"), held_by("alice"));
        assert_eq!(group_hold(&request, "qam-cloud", "bob"), held_by("alice"));
    }

    #[test]
    fn comment_naming_another_user_does_not_hold() {
        // The comment links qam-sle to alice, so bob's open review is not this
        // group's however recent it is.
        let reviews = grp("qam-sle", "accepted", &[("alice", T_ASSIGNED, ACCEPTED)])
            + &usr("bob", "new", T_REASSIGNED, &reassigned("qam-sle", "alice"));
        assert_eq!(
            group_hold(&req(&reviews), "qam-sle", "carol"),
            GroupHold::Approved {
                by: Some(("alice".to_owned(), T_ASSIGNED.to_owned()))
            }
        );
    }

    #[test]
    fn reassign_after_approval_is_held_by_the_assignee() {
        let reviews = grp(
            "qam-sle",
            "accepted",
            &[
                ("alice", T_ASSIGNED, ACCEPTED),
                ("bob", T_REASSIGNED, REOPENED),
            ],
        ) + &usr(
            "alice",
            "accepted",
            T_APPROVED,
            "[oscqam] Approving for alice.",
        ) + &usr("bob", "new", T_REASSIGNED, &reassigned("qam-sle", "bob"));
        let request = req(&reviews);
        assert_eq!(group_hold(&request, "qam-sle", "carol"), held_by("bob"));
        assert_eq!(group_hold(&request, "qam-sle", "bob"), GroupHold::HeldByMe);
        assert_eq!(group_hold(&request, "qam-sle", "alice"), held_by("bob"));
    }

    /// The time printed is the group review's own accept event, never the
    /// approver's user review: that `when` is theirs, not the group's.
    #[test]
    fn finished_user_review_leaves_the_group_approved() {
        let reviews = grp("qam-sle", "accepted", &[("alice", T_ASSIGNED, ACCEPTED)])
            + &usr(
                "alice",
                "accepted",
                T_APPROVED,
                "[oscqam] Approving for alice.",
            );
        assert_eq!(
            group_hold(&req(&reviews), "qam-sle", "bob"),
            GroupHold::Approved {
                by: Some(("alice".to_owned(), T_ASSIGNED.to_owned()))
            }
        );
    }

    #[test]
    fn directly_accepted_group_is_approved_by_the_event_actor() {
        let reviews = grp("qam-sle", "accepted", &[("bob", T_APPROVED, ACCEPTED)]);
        assert_eq!(
            group_hold(&req(&reviews), "qam-sle", "alice"),
            GroupHold::Approved {
                by: Some(("bob".to_owned(), T_APPROVED.to_owned()))
            }
        );
    }

    #[test]
    fn accepted_group_without_history_is_approved_without_actor() {
        let reviews = "<review state='accepted' by_group='qam-sle'/>";
        assert_eq!(
            group_hold(&req(reviews), "qam-sle", "alice"),
            GroupHold::Approved { by: None }
        );
    }

    /// Pins the liveness filter: a declined review is not open, so it neither
    /// holds the group nor stops it reading as approved.
    #[test]
    fn declined_user_review_does_not_hold() {
        let reviews = grp("qam-sle", "accepted", &[("bob", T_ASSIGNED, ACCEPTED)])
            + &usr("bob", "declined", T_APPROVED, &reassigned("qam-sle", "bob"));
        assert_eq!(
            group_hold(&req(&reviews), "qam-sle", "alice"),
            GroupHold::Approved {
                by: Some(("bob".to_owned(), T_ASSIGNED.to_owned()))
            }
        );
    }

    #[test]
    fn two_open_user_reviews_prefer_the_last_event_actor() {
        // bob's user review carries the older `when`, so only the history actor
        // picks him; the `when` fallback would pick alice.
        let reviews = grp(
            "qam-sle",
            "accepted",
            &[
                ("alice", T_ASSIGNED, ACCEPTED),
                ("bob", T_REASSIGNED, REOPENED),
            ],
        ) + &usr(
            "alice",
            "new",
            T_REASSIGNED,
            &reassigned("qam-sle", "alice"),
        ) + &usr("bob", "new", T_ASSIGNED, &reassigned("qam-sle", "bob"));
        let request = req(&reviews);
        assert_eq!(group_hold(&request, "qam-sle", "carol"), held_by("bob"));
        assert_eq!(
            group_hold(&request, "qam-sle", "alice"),
            GroupHold::HeldByMe
        );
    }

    #[test]
    fn open_user_reviews_without_group_history_fall_back_to_the_latest() {
        let reviews = "<review state='accepted' by_group='qam-sle'/>".to_owned()
            + &usr("alice", "new", T_ASSIGNED, &reassigned("qam-sle", "alice"))
            + &usr("bob", "new", T_REASSIGNED, &reassigned("qam-sle", "bob"));
        assert_eq!(
            group_hold(&req(&reviews), "qam-sle", "carol"),
            held_by("bob")
        );
    }

    #[test]
    fn caller_holding_another_group_is_not_the_holder() {
        let reviews = grp("qam-sle", "accepted", &[("bob", T_ASSIGNED, ACCEPTED)])
            + &grp("qam-cloud", "accepted", &[("alice", T_APPROVED, ACCEPTED)])
            + &usr("bob", "new", T_ASSIGNED, &reassigned("qam-sle", "bob"))
            + &usr(
                "alice",
                "new",
                T_APPROVED,
                &reassigned("qam-cloud", "alice"),
            );
        assert_eq!(
            group_hold(&req(&reviews), "qam-sle", "alice"),
            held_by("bob")
        );
    }

    #[test]
    fn commentless_open_review_by_the_last_actor_still_holds() {
        // The guess the `HeldBy` message no longer over-states: the document
        // links alice to the group by nothing but the group review's last actor.
        let reviews = grp("qam-sle", "accepted", &[("alice", T_ASSIGNED, ACCEPTED)])
            + &usr("alice", "new", T_REASSIGNED, "");
        assert_eq!(
            group_hold(&req(&reviews), "qam-sle", "bob"),
            held_by("alice")
        );
    }

    /// Pins `latest_key`'s bucket: an unparseable `when` never outranks a
    /// placeable one, however late it sits in the document.
    #[test]
    fn an_unplaceable_event_never_outranks_a_placeable_one() {
        let reviews = grp(
            "qam-sle",
            "accepted",
            &[
                ("alice", T_APPROVED, ACCEPTED),
                ("bob", "no such instant", ACCEPTED),
            ],
        );
        assert_eq!(
            group_hold(&req(&reviews), "qam-sle", "carol"),
            GroupHold::Approved {
                by: Some(("alice".to_owned(), T_APPROVED.to_owned()))
            }
        );
    }

    #[test]
    fn two_accepted_group_reviews_read_the_later_one() {
        // Written out because `grp()` emits no `when`: here document order and
        // `latest_key(&r.when)` disagree, so the rule is pinned, not assumed.
        let reviews = format!(
            "<review state='accepted' when='{T_REASSIGNED}' by_group='qam-sle'>\
             <history who='alice' when='{T_ASSIGNED}'>\
             <description>{ACCEPTED}</description></history></review>\
             <review state='accepted' when='{T_ASSIGNED}' by_group='qam-sle'>\
             <history who='bob' when='{T_APPROVED}'>\
             <description>{ACCEPTED}</description></history></review>"
        );
        assert_eq!(
            group_hold(&req(&reviews), "qam-sle", "carol"),
            GroupHold::Approved {
                by: Some(("bob".to_owned(), T_APPROVED.to_owned()))
            }
        );
    }
}
