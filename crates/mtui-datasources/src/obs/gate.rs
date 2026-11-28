//! A request's QA review gate, read without acting on it: who holds each qam
//! group review and whether its verdict is still owed.
//!
//! The holder is `group_hold`'s answer; where `assignments_for_user` names
//! someone else the owner is disputed rather than either answer (the `hold`
//! module lists the cases where the two differ).

use std::collections::BTreeSet;

use mtui_types::RequestReviewID;

use crate::obs::client::ObsClient;
use crate::obs::errors::ObsError;
use crate::obs::hold::{GroupHold, group_hold};
use crate::obs::inference::assignments_for_user;
use crate::obs::models::{Request, Review, is_qam_automation_group, is_qam_group};
use crate::obs::qam::get_request;

/// Who holds one qam group's review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupOwner {
    /// Nobody holds it.
    Nobody,
    /// This user holds it.
    User(String),
    /// An open user review holds it, but its `by_user` is blank.
    Unnamed,
    /// The open reviews and the group review's history disagree: `hold` is
    /// the open-review answer, `history` the users its history assigns.
    Disputed {
        /// The holder by the open user review, if any.
        hold: Option<String>,
        /// The users the group review's history leaves assigned.
        history: Vec<String>,
    },
}

/// Where one qam group's verdict stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupVerdict {
    /// The group review is open and nobody has taken it.
    Open,
    /// An assign accepted the group review and opened a user review that is
    /// still open: the verdict is owed by that user.
    Assigned,
    /// The group review is accepted and no user review the assign opened is
    /// still open.
    Accepted,
}

/// One qam group review on a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupGate {
    /// The qam group.
    pub group: String,
    /// An automation group (`qam-auto`, `qam-openqa`): a bot reviews for it.
    pub automated: bool,
    /// Who holds it.
    pub owner: GroupOwner,
    /// Where its verdict stands.
    pub verdict: GroupVerdict,
    /// When it got there, as OBS wrote it; `None` when the document says not.
    pub since: Option<String>,
}

/// A request's state and its qam group reviews.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestGate {
    /// The request's own state (`new`, `review`, `accepted`, `declined`, ...).
    pub state: String,
    /// When the request entered `state`; `None` when absent.
    pub state_since: Option<String>,
    /// When the request was created; `None` when absent.
    pub created: Option<String>,
    /// The first action's source project.
    pub source_project: Option<String>,
    /// One entry per qam group, automation groups included, with a live
    /// (`new`/`accepted`) review, in document order: the groups
    /// [`open_reviews_in`](crate::obs::qam::open_reviews_in) classifies.
    pub groups: Vec<GroupGate>,
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_owned())
}

/// The users the group review history assigns to `group`.
fn inferred_holders(request: &Request, group: &str) -> Vec<String> {
    let candidates: BTreeSet<&str> = request
        .reviews
        .iter()
        .filter_map(|r| r.by_user.as_deref())
        .chain(
            request
                .reviews
                .iter()
                .flat_map(|r| r.history.iter().map(|e| e.who.as_str())),
        )
        .filter(|u| !u.is_empty())
        .collect();
    candidates
        .into_iter()
        .filter(|u| {
            assignments_for_user(request, u)
                .iter()
                .any(|a| a.group == group)
        })
        .map(str::to_owned)
        .collect()
}

fn group_gate(request: &Request, group: &str, live: &[&Review]) -> GroupGate {
    // No caller login: `HeldByMe` then means an open review with a blank
    // `by_user`, held by someone the document does not name.
    let hold = group_hold(request, group, "");
    let held_by = match &hold {
        GroupHold::HeldBy { user } => Some(user.as_str()),
        GroupHold::HeldByMe => Some(""),
        _ => None,
    };
    let history = inferred_holders(request, group);
    let owner = match (held_by, history.as_slice()) {
        (None, []) => GroupOwner::Nobody,
        (Some(""), []) => GroupOwner::Unnamed,
        (Some(h), [only]) if !h.is_empty() && h == only => GroupOwner::User(h.to_owned()),
        _ => GroupOwner::Disputed {
            hold: held_by.and_then(non_empty),
            history,
        },
    };
    let (verdict, since) = match (&hold, held_by) {
        (_, Some(user)) => {
            let since = request
                .reviews
                .iter()
                .filter(|r| r.state == "new" && r.by_user.as_deref() == Some(user))
                .find_map(|r| non_empty(&r.when));
            (GroupVerdict::Assigned, since)
        }
        (GroupHold::Approved { by }, None) => (
            GroupVerdict::Accepted,
            by.as_ref().and_then(|(_, when)| non_empty(when)),
        ),
        _ => {
            let open = live.iter().rev().find(|r| r.state == "new");
            (GroupVerdict::Open, open.and_then(|r| non_empty(&r.when)))
        }
    };
    GroupGate {
        group: group.to_owned(),
        automated: is_qam_automation_group(group),
        owner,
        verdict,
        since,
    }
}

/// The gate of an already-fetched `withfullhistory` request document.
#[must_use]
pub fn request_gate(request: &Request) -> RequestGate {
    let mut seen = BTreeSet::new();
    let groups = request
        .reviews
        .iter()
        .filter_map(|r| r.by_group.as_deref())
        .filter(|g| (is_qam_group(g) || is_qam_automation_group(g)) && seen.insert(*g))
        .filter_map(|group| {
            let live: Vec<&Review> = request
                .reviews
                .iter()
                .filter(|r| r.by_group.as_deref() == Some(group))
                .filter(|r| matches!(r.state.as_str(), "new" | "accepted"))
                .collect();
            (!live.is_empty()).then(|| group_gate(request, group, &live))
        })
        .collect();
    RequestGate {
        state: request.state.clone(),
        state_since: non_empty(&request.state_when),
        created: non_empty(&request.created),
        source_project: request.src_project.clone(),
        groups,
    }
}

/// GET `request/{id}?withfullhistory=1` and read its gate. Read-only.
///
/// # Errors
///
/// A transport/API error from the GET, or [`ObsError::Parse`].
pub async fn read_request_gate(
    client: &ObsClient,
    rrid: &RequestReviewID,
) -> Result<RequestGate, ObsError> {
    Ok(request_gate(&get_request(client, rrid).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::models::parse_request;
    use crate::obs::qam::{OpenReview, open_reviews_in};

    const T1: &str = "2026-09-01T00:33:57";
    const T2: &str = "2026-09-02T07:25:47";

    fn gate(state: &str, reviews: &str) -> RequestGate {
        request_gate(
            &parse_request(&format!(
                "<request id='1'><state name='{state}' when='{T2}' created='{T1}'/>{reviews}</request>"
            ))
            .unwrap(),
        )
    }

    fn accepted_by(group: &str, who: &str, when: &str) -> String {
        format!(
            "<review state='accepted' by_group='{group}'><history who='{who}' when='{when}'>\
             <description>Review got accepted</description></history></review>"
        )
    }

    fn user_review(user: &str, state: &str, group: &str) -> String {
        format!(
            "<review state='{state}' when='{T2}' who='{user}' by_user='{user}'>\
             <history who='{user}' when='{T1}'><description>Review got assigned</description>\
             </history><comment>reassigned review for group {group} to user {user}</comment></review>"
        )
    }

    #[test]
    fn reads_the_request_state_and_its_times() {
        let g = gate("review", "");
        assert_eq!(g.state, "review");
        assert_eq!(g.state_since.as_deref(), Some(T2));
        assert_eq!(g.created.as_deref(), Some(T1));
    }

    #[test]
    fn an_open_group_review_is_open_and_nobodys() {
        let g = gate("review", "<review state='new' by_group='qam-sle'/>");
        assert_eq!(
            g.groups,
            vec![GroupGate {
                group: "qam-sle".to_owned(),
                automated: false,
                owner: GroupOwner::Nobody,
                verdict: GroupVerdict::Open,
                since: None,
            }]
        );
    }

    #[test]
    fn an_assign_holds_the_group_and_owes_the_verdict() {
        let reviews = accepted_by("qam-sle", "alice", T1) + &user_review("alice", "new", "qam-sle");
        let g = gate("review", &reviews);
        assert_eq!(g.groups[0].owner, GroupOwner::User("alice".to_owned()));
        assert_eq!(g.groups[0].verdict, GroupVerdict::Assigned);
        assert_eq!(g.groups[0].since.as_deref(), Some(T2));
    }

    #[test]
    fn a_finished_user_review_leaves_the_group_accepted() {
        let reviews = accepted_by("qam-sle", "alice", T1)
            + &user_review("alice", "accepted", "qam-sle").replace(
                "</history>",
                &format!(
                    "</history><history who='alice' when='{T2}'>\
                     <description>Review got accepted</description></history>"
                ),
            );
        let g = gate("review", &reviews);
        assert_eq!(g.groups[0].verdict, GroupVerdict::Accepted);
        assert_eq!(g.groups[0].owner, GroupOwner::Nobody);
        assert_eq!(g.groups[0].since.as_deref(), Some(T1));
    }

    /// A group accepted with no user review: nobody holds it by its open
    /// reviews, but its history assigns the actor, so the owner is disputed.
    #[test]
    fn history_naming_someone_the_reviews_do_not_is_disputed() {
        let g = gate("review", &accepted_by("qam-sle", "bob", T1));
        assert_eq!(g.groups[0].verdict, GroupVerdict::Accepted);
        assert_eq!(
            g.groups[0].owner,
            GroupOwner::Disputed {
                hold: None,
                history: vec!["bob".to_owned()],
            }
        );
    }

    /// Automation groups are kept and flagged, as `open_reviews_in` lists
    /// them, so both views classify the same groups the same way.
    #[test]
    fn two_qam_groups_are_read_apart_and_automation_is_flagged() {
        let reviews = "<review state='new' by_group='qam-cloud'/>".to_owned()
            + "<review state='new' by_group='qam-openqa'/>"
            + "<review state='new' by_group='maintenance-team'/>"
            + "<review state='declined' by_group='qam-gone'/>"
            + &accepted_by("qam-sle", "alice", T1)
            + &user_review("alice", "new", "qam-sle");
        let g = gate("review", &reviews);
        let names: Vec<(&str, bool, GroupVerdict)> = g
            .groups
            .iter()
            .map(|x| (x.group.as_str(), x.automated, x.verdict))
            .collect();
        assert_eq!(
            names,
            vec![
                ("qam-cloud", false, GroupVerdict::Open),
                ("qam-openqa", true, GroupVerdict::Open),
                ("qam-sle", false, GroupVerdict::Assigned)
            ]
        );
        let request = parse_request(&format!(
            "<request id='1'><state name='review'/>{reviews}</request>"
        ))
        .unwrap();
        let open: Vec<String> = open_reviews_in(&request, "nobody")
            .into_iter()
            .map(|o| match o {
                OpenReview::Group(g) | OpenReview::AutomatedGroup(g) => g,
                OpenReview::User(u) => u,
            })
            .collect();
        let gated_open: Vec<&str> = g
            .groups
            .iter()
            .filter(|x| x.verdict == GroupVerdict::Open)
            .map(|x| x.group.as_str())
            .collect();
        assert_eq!(open, gated_open);
    }

    /// `fuzz/seed_corpus/obs_xml/request_full_history.xml`, copied: a crate
    /// may not read outside its package.
    const SEED_REQUEST: &str = r#"<request id="42">
  <action type="maintenance_release">
    <source project="SUSE:Maintenance:130" package="pkg.SUSE_SLE-12_Update"/>
    <target project="SUSE:SLE-12:Update" package="pkg.130"/>
  </action>
  <review state="review" when="2014-11-14T11:12:53" who="anon" by_user="anon"/>
  <review state="accepted" by_group="qam-sle">
    <history who="alice" when="2017-09-06T08:06:39">
      <description>Review got accepted</description>
      <comment>review for group qam-sle</comment>
    </history>
  </review>
  <review state="new" by_group="qam-cloud"/>
  <state name="review" who="anon" when="2014-12-01T14:46:23"/>
</request>"#;

    /// An accepted event without a `when` leaves the time unknown, never a
    /// known empty one.
    #[test]
    fn an_accepted_event_without_a_time_has_no_since() {
        let reviews = "<review state='accepted' by_group='qam-sle'><history who='alice'>\
                       <description>Review got accepted</description></history></review>";
        let g = gate("review", reviews);
        assert_eq!(g.groups[0].verdict, GroupVerdict::Accepted);
        assert_eq!(g.groups[0].since, None);
    }

    /// An open user review with a blank reviewer holds the group but names
    /// nobody: unnamed, never a known empty login nor a dispute.
    #[test]
    fn a_blank_holder_is_unnamed_not_a_login() {
        let reviews = "<review state='accepted' by_group='qam-sle'><history who=''>\
                       <description>Review got accepted</description></history></review>\
                       <review state='new' by_user=''/>";
        let g = gate("review", reviews);
        assert_eq!(g.groups[0].owner, GroupOwner::Unnamed);
        assert_eq!(g.groups[0].verdict, GroupVerdict::Assigned);
    }

    /// The fuzz seed request: an accepted qam-sle whose history assigns
    /// alice, an open qam-cloud, and a state without `created`.
    #[test]
    fn the_seed_request_reads_its_groups_and_no_creation_time() {
        let g = request_gate(&parse_request(SEED_REQUEST).unwrap());
        assert_eq!(g.state, "review");
        assert_eq!(g.state_since.as_deref(), Some("2014-12-01T14:46:23"));
        assert_eq!(g.created, None);
        assert_eq!(g.source_project.as_deref(), Some("SUSE:Maintenance:130"));
        let groups: Vec<(&str, &GroupOwner, GroupVerdict)> = g
            .groups
            .iter()
            .map(|x| (x.group.as_str(), &x.owner, x.verdict))
            .collect();
        assert_eq!(
            groups,
            vec![
                (
                    "qam-sle",
                    &GroupOwner::Disputed {
                        hold: None,
                        history: vec!["alice".to_owned()]
                    },
                    GroupVerdict::Accepted
                ),
                ("qam-cloud", &GroupOwner::Nobody, GroupVerdict::Open),
            ]
        );
    }
}
