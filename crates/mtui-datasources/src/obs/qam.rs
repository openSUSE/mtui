//! The five QAM review operations as direct OBS REST calls (no `osc`).
//!
//! Ported from upstream `mtui/data_sources/obs/qam.py`. Each function performs
//! the OBS calls for one operation and returns `Err` on any failure or refused
//! precondition; the never-raise `OSC` facade (G1g) wraps every call and folds a
//! returned error into the `false` its callers expect. Semantics mirror the
//! `osc qam` plugin exactly, including the awkward parts: single-group
//! auto-inference, the ">=1 own assignment" unassign guard, the refused
//! group-approve, `by_user` reject with the `MAINT:RejectReason`
//! read-modify-write, and the `qam.suse.de` preconditions (skipped for PI/SLFO).
//! The `[oscqam] ` prefix is applied to approve/reject comments only.
//!
//! Unlike upstream (which takes a `Config`), these functions take the URL/verify
//! values they need as explicit parameters — the `[obs]` config table and the
//! facade that binds these from a resolved `Config` land in G1g. This keeps the
//! ops self-contained and testable now, mirroring how [`ObsClient`] itself takes
//! an explicit API URL / timeout / verify posture rather than a `Config`.

use mtui_config::SslVerify;

use mtui_types::{RequestKind, RequestReviewID};

use crate::obs::client::ObsClient;
use crate::obs::errors::ObsError;
use crate::obs::inference::assignments_for_user;
use crate::obs::models::{
    self, REJECT_REASON_NAME, REJECT_REASON_NAMESPACE, build_reject_reason_body, is_qam_group,
    parse_group_directory, parse_reject_reason_values, parse_request, parse_request_collection,
};

const PREFIX: &str = "[oscqam] ";

/// PI/SLFO requests carry no maintenance testreport or `MAINT` attribute.
fn is_slfo(rrid: &RequestReviewID) -> bool {
    matches!(rrid.kind, RequestKind::Pi | RequestKind::Slfo)
}

/// The request id used in OBS paths (`rrid.review_id`).
fn reqid(rrid: &RequestReviewID) -> String {
    rrid.review_id.to_string()
}

/// The fancy testreport log URL for approve/reject comments, mirroring upstream
/// `_fancy_url` (`fancy_reports_url.rstrip('/') + "/" + rrid + "/log"`).
fn fancy_url(fancy_reports_url: &str, rrid: &RequestReviewID) -> String {
    format!("{}/{rrid}/log", fancy_reports_url.trim_end_matches('/'))
}

/// GET `request/{id}?withfullhistory=1` and parse it.
async fn get_request(
    client: &ObsClient,
    rrid: &RequestReviewID,
) -> Result<models::Request, ObsError> {
    let response = client
        .get(
            &format!("request/{}", reqid(rrid)),
            &[("withfullhistory", "1".to_owned())],
        )
        .await?;
    parse_request(&response)
}

/// POST a `changereviewstate` (accept/decline) `by_user`.
async fn changereviewstate(
    client: &ObsClient,
    rrid: &RequestReviewID,
    newstate: &str,
    user: &str,
    comment: &str,
) -> Result<(), ObsError> {
    client
        .post(
            &format!("request/{}", reqid(rrid)),
            &[
                ("cmd", "changereviewstate".to_owned()),
                ("newstate", newstate.to_owned()),
                ("by_user", user.to_owned()),
            ],
            comment,
        )
        .await?;
    Ok(())
}

// --------------------------------------------------------------------------- //
// comment                                                                      //
// --------------------------------------------------------------------------- //

/// POST a raw (unprefixed) comment to the request.
///
/// # Errors
///
/// Returns [`ObsError::Op`] for a whitespace-only comment, or a transport/API
/// error from the POST.
pub async fn comment(
    client: &ObsClient,
    rrid: &RequestReviewID,
    text: &str,
) -> Result<(), ObsError> {
    if text.trim().is_empty() {
        return Err(ObsError::Op("refusing to post an empty comment".to_owned()));
    }
    client
        .post(&format!("comments/request/{}", reqid(rrid)), &[], text)
        .await?;
    Ok(())
}

// --------------------------------------------------------------------------- //
// assign                                                                       //
// --------------------------------------------------------------------------- //

/// Resolve the group(s) to assign: explicit ones pass through; otherwise
/// auto-infer the single qam group the user is in that has an open (`new`)
/// review, refusing if that is not exactly one.
async fn resolve_assign_groups(
    client: &ObsClient,
    request: &models::Request,
    user: &str,
    groups: &[String],
) -> Result<Vec<String>, ObsError> {
    if !groups.is_empty() {
        return Ok(groups.to_vec());
    }
    let directory = client.get("group", &[("login", user.to_owned())]).await?;
    let user_groups: std::collections::HashSet<String> =
        parse_group_directory(&directory)?.into_iter().collect();
    let mut candidates: Vec<String> = request
        .reviews
        .iter()
        .filter_map(|r| r.by_group.as_deref())
        .filter(|g| is_qam_group(g))
        .filter(|g| {
            request
                .reviews
                .iter()
                .any(|r| r.by_group.as_deref() == Some(g) && r.state == "new")
        })
        .filter(|g| user_groups.contains(*g))
        .map(ToOwned::to_owned)
        .collect();
    candidates.sort();
    candidates.dedup();
    if candidates.len() != 1 {
        let listed = if candidates.is_empty() {
            "none".to_owned()
        } else {
            candidates.join(", ")
        };
        return Err(ObsError::Op(format!(
            "cannot auto-infer a single qam group to assign {user} to \
             (open groups the user is in: {listed}); pass -g"
        )));
    }
    Ok(candidates)
}

/// Refuse if a related qam request was declined and `user` was not on it.
async fn check_previous_rejects(
    client: &ObsClient,
    request: &models::Request,
    user: &str,
) -> Result<(), ObsError> {
    let Some(src_project) = request.src_project.as_deref() else {
        return Ok(());
    };
    let collection = client
        .get(
            "request",
            &[
                ("project", src_project.to_owned()),
                ("view", "collection".to_owned()),
                ("withfullhistory", "1".to_owned()),
            ],
        )
        .await?;
    let related: Vec<models::Request> = parse_request_collection(&collection)?
        .into_iter()
        .filter(|r| {
            r.reviews
                .iter()
                .any(|rev| rev.by_group.as_deref().is_some_and(is_qam_group))
        })
        .collect();
    let declined: Vec<&models::Request> =
        related.iter().filter(|r| r.state == "declined").collect();
    if declined.is_empty() {
        return Ok(());
    }
    let prior_reviewer = declined
        .iter()
        .flat_map(|r| r.reviews.iter())
        .any(|rev| rev.by_user.as_deref() == Some(user));
    if !prior_reviewer {
        return Err(ObsError::Op(format!(
            "request was previously declined and {user} was not a prior \
             reviewer; refusing to assign (a re-review needs the original \
             reviewer)"
        )));
    }
    Ok(())
}

/// Assign the review to `user` for the resolved group(s).
///
/// # Errors
///
/// Returns [`ObsError::Op`] if the request is not open for review, the group
/// cannot be auto-inferred, no testreport exists (non-SLFO), or a previous
/// decline blocks the re-review; or a transport/API error.
pub async fn assign(
    client: &ObsClient,
    reports_url: &str,
    ssl_verify: &SslVerify,
    rrid: &RequestReviewID,
    user: &str,
    groups: &[String],
) -> Result<(), ObsError> {
    let request = get_request(client, rrid).await?;
    // Mirror the plugin's Request.OPEN_STATES = ("new", "review"); OBS reports
    // "new" while a request still has an open review it has not moved on from.
    if request.state != "new" && request.state != "review" {
        return Err(ObsError::Op(format!(
            "request {} is not open for review (state={:?}); refusing to assign",
            request.reqid, request.state
        )));
    }
    let resolved = resolve_assign_groups(client, &request, user, groups).await?;
    if !is_slfo(rrid) {
        if super::preconditions::fetch_testreport_log(reports_url, ssl_verify, rrid)
            .await
            .is_none()
        {
            return Err(ObsError::Op(format!(
                "no testreport found for {rrid} on qam.suse.de; refusing to \
                 assign (the report generator may still be running)"
            )));
        }
        check_previous_rejects(client, &request, user).await?;
    }
    for group in resolved {
        client
            .post(
                &format!("request/{}", reqid(rrid)),
                &[
                    ("cmd", "assignreview".to_owned()),
                    ("reviewer", user.to_owned()),
                    ("by_group", group.clone()),
                ],
                &format!("Assigning {user} to {group} for {rrid}."),
            )
            .await?;
    }
    Ok(())
}

// --------------------------------------------------------------------------- //
// unassign                                                                     //
// --------------------------------------------------------------------------- //

/// Revert `user`'s assignment for the resolved (or explicit) group(s).
///
/// # Errors
///
/// Returns [`ObsError::Op`] if `user` holds no assignment; or a transport/API
/// error.
pub async fn unassign(
    client: &ObsClient,
    rrid: &RequestReviewID,
    user: &str,
    groups: &[String],
) -> Result<(), ObsError> {
    let request = get_request(client, rrid).await?;
    let own = assignments_for_user(&request, user);
    if own.is_empty() {
        return Err(ObsError::Op(format!(
            "{user} holds no review assignment on request {}; nothing to unassign",
            request.reqid
        )));
    }
    let resolved = if groups.is_empty() {
        let mut inferred: Vec<String> = own.into_iter().map(|a| a.group).collect();
        inferred.sort();
        inferred.dedup();
        inferred
    } else {
        groups.to_vec()
    };
    for group in resolved {
        client
            .post(
                &format!("request/{}", reqid(rrid)),
                &[
                    ("cmd", "assignreview".to_owned()),
                    ("revert", "1".to_owned()),
                    ("reviewer", user.to_owned()),
                    ("by_group", group.clone()),
                ],
                &format!("Unassigning {user} from {rrid} for group {group}."),
            )
            .await?;
    }
    Ok(())
}

// --------------------------------------------------------------------------- //
// approve (user path only)                                                     //
// --------------------------------------------------------------------------- //

/// Accept the review by user; group-approve is refused (parity).
///
/// # Errors
///
/// Returns [`ObsError::Op`] if groups are given (group-approve refused), the
/// user is not assigned, or the testreport is not `PASSED` (non-SLFO); or a
/// transport/API error.
pub async fn approve(
    client: &ObsClient,
    reports_url: &str,
    fancy_reports_url: &str,
    ssl_verify: &SslVerify,
    rrid: &RequestReviewID,
    user: &str,
    groups: &[String],
) -> Result<(), ObsError> {
    if !groups.is_empty() {
        return Err(ObsError::Op(
            "group approval is not supported by the native OBS backend \
             (it can leave the update in an inconsistent state); approve the \
             review assigned to you without -g"
                .to_owned(),
        ));
    }
    let request = get_request(client, rrid).await?;
    if assignments_for_user(&request, user).is_empty() {
        return Err(ObsError::Op(format!(
            "{user} is not assigned to request {}; assign it to yourself before approving",
            request.reqid
        )));
    }
    if !is_slfo(rrid) {
        let log = super::preconditions::fetch_testreport_log(reports_url, ssl_verify, rrid).await;
        if log.is_none_or(|log| super::preconditions::summary(&log) != "PASSED") {
            return Err(ObsError::Op(format!(
                "testreport for {rrid} is not PASSED; refusing to approve"
            )));
        }
    }
    let comment = format!(
        "{PREFIX}Approving {rrid} for {user}. Testreport: {}",
        fancy_url(fancy_reports_url, rrid)
    );
    changereviewstate(client, rrid, "accepted", user, &comment).await
}

// --------------------------------------------------------------------------- //
// reject (always by_user)                                                      //
// --------------------------------------------------------------------------- //

/// Append `<reqid>:<reason>` to the source project's `MAINT:RejectReason`.
async fn write_reject_reason(
    client: &ObsClient,
    request: &models::Request,
    rrid: &RequestReviewID,
    reason: &str,
) -> Result<(), ObsError> {
    let Some(src_project) = request.src_project.as_deref() else {
        return Ok(());
    };
    let path =
        format!("source/{src_project}/_attribute/{REJECT_REASON_NAMESPACE}:{REJECT_REASON_NAME}");
    let existing_body = client.get(&path, &[]).await?;
    let mut merged = parse_reject_reason_values(&existing_body)?;
    merged.push(format!("{}:{reason}", reqid(rrid)));
    client
        .post(&path, &[], &build_reject_reason_body(&merged))
        .await?;
    Ok(())
}

/// Decline the review by user, recording the reject reason attribute.
///
/// `groups` is ignored (native reject is always `by_user`); a non-empty value
/// is logged at INFO. The reviewer's `message` is a required parameter but, for
/// parity, is deliberately **not** recorded in the decline comment.
///
/// # Errors
///
/// Returns [`ObsError::Op`] if the testreport is not `FAILED` or has no comment
/// (non-SLFO); or a transport/API error.
// The explicit-params design (no `Config` coupling until the G1g facade) means
// this faithful port of upstream `reject` carries one arg past clippy's default
// threshold; the facade will bundle the URL/verify values.
#[allow(clippy::too_many_arguments)]
pub async fn reject(
    client: &ObsClient,
    reports_url: &str,
    fancy_reports_url: &str,
    ssl_verify: &SslVerify,
    rrid: &RequestReviewID,
    user: &str,
    groups: &[String],
    reason: &str,
    _message: &str,
) -> Result<(), ObsError> {
    if !groups.is_empty() {
        tracing::info!("reject ignores -g/--group (native reject is always by_user)");
    }
    let request = get_request(client, rrid).await?;
    if !is_slfo(rrid) {
        let log = super::preconditions::fetch_testreport_log(reports_url, ssl_verify, rrid).await;
        let log = match log {
            Some(log) if super::preconditions::summary(&log) == "FAILED" => log,
            _ => {
                return Err(ObsError::Op(format!(
                    "testreport for {rrid} is not FAILED; refusing to reject"
                )));
            }
        };
        if super::preconditions::comment(&log).is_empty() {
            return Err(ObsError::Op(format!(
                "testreport for {rrid} has no comment; refusing to reject"
            )));
        }
        write_reject_reason(client, &request, rrid, reason).await?;
    }
    // Parity: the reviewer's -M message is not recorded in the decline comment.
    let comment = format!(
        "{PREFIX}Declining request {rrid} for {user}. See Testreport: {}",
        fancy_url(fancy_reports_url, rrid)
    );
    changereviewstate(client, rrid, "declined", user, &comment).await
}
