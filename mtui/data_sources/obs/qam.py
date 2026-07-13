"""The five QAM review operations as direct OBS REST calls (no osc).

Each function performs the OBS calls for one operation and raises on any
failure or refused precondition; the :class:`~mtui.data_sources.oscqam.OSC`
facade wraps every call and converts a raised error into the ``False`` its
callers expect. Semantics mirror the ``osc qam`` plugin exactly, including
the awkward parts: single-group auto-inference, the ">=1 own assignment"
unassign guard, the refused group-approve, ``by_user`` reject with the
``MAINT:RejectReason`` read-modify-write, and the qam.suse.de preconditions
(skipped for PI/SLFO). ``[oscqam] `` prefixes approve/reject comments only.
"""

from __future__ import annotations

from logging import getLogger
from typing import TYPE_CHECKING

from ...types.enums import RequestKind
from . import models, preconditions
from .errors import ObsError
from .inference import assignments_for_user

if TYPE_CHECKING:
    from ...support.config import Config
    from ...types.rrid import RequestReviewID
    from .client import ObsClient

logger = getLogger("mtui.data_sources.obs.qam")

_PREFIX = "[oscqam] "
_ASSIGN_MSG = "Assigning {user} to {group} for {request}."
_UNASSIGN_MSG = "Unassigning {user} from {request} for group {group}."
_APPROVE_MSG = "Approving {request} for {user}. Testreport: {url}"
_DECLINE_MSG = "Declining request {request} for {user}. See Testreport: {url}"


def _is_slfo(rrid: RequestReviewID) -> bool:
    """PI/SLFO requests carry no maintenance testreport or MAINT attribute."""
    return rrid.kind in (RequestKind.PI, RequestKind.SLFO)


def _reqid(rrid: RequestReviewID) -> str:
    return str(rrid.review_id)


def _fancy_url(config: Config, rrid: RequestReviewID) -> str:
    return f"{config.fancy_reports_url.rstrip('/')}/{rrid}/log"


def _get_request(client: ObsClient, rrid: RequestReviewID) -> models.Request:
    response = client.get(f"request/{_reqid(rrid)}", params={"withfullhistory": 1})
    return models.parse_request(response.text)


def _changereviewstate(
    client: ObsClient, rrid: RequestReviewID, newstate: str, user: str, comment: str
) -> None:
    client.post(
        f"request/{_reqid(rrid)}",
        params={"cmd": "changereviewstate", "newstate": newstate, "by_user": user},
        body=comment,
    )


# --------------------------------------------------------------------------- #
# comment                                                                      #
# --------------------------------------------------------------------------- #
def comment(client: ObsClient, rrid: RequestReviewID, text: str) -> None:
    """POST a raw (unprefixed) comment to the request."""
    if not text.strip():
        raise ObsError("refusing to post an empty comment")
    client.post(f"comments/request/{_reqid(rrid)}", body=text)


# --------------------------------------------------------------------------- #
# assign                                                                       #
# --------------------------------------------------------------------------- #
def _resolve_assign_groups(
    client: ObsClient, request: models.Request, user: str, groups: list[str]
) -> list[str]:
    if groups:
        return groups
    directory = client.get("group", params={"login": user})
    user_groups = set(models.parse_group_directory(directory.text))
    open_qam = {
        r.by_group
        for r in request.reviews
        if r.by_group and models.is_qam_group(r.by_group) and r.state == "new"
    }
    candidates = sorted(user_groups & open_qam)
    if len(candidates) != 1:
        raise ObsError(
            f"cannot auto-infer a single qam group to assign {user} to "
            f"(open groups the user is in: {candidates or 'none'}); pass -g"
        )
    return candidates


def _check_previous_rejects(
    client: ObsClient, request: models.Request, user: str
) -> None:
    """Refuse if a related qam request was declined and ``user`` was not on it."""
    if request.src_project is None:
        return
    collection = client.get(
        "request",
        params={
            "project": request.src_project,
            "view": "collection",
            "withfullhistory": 1,
        },
    )
    related = [
        r
        for r in models.parse_request_collection(collection.text)
        if any(rev.by_group and models.is_qam_group(rev.by_group) for rev in r.reviews)
    ]
    declined = [r for r in related if r.state == "declined"]
    if not declined:
        return
    prior_reviewer = any(rev.by_user == user for r in declined for rev in r.reviews)
    if not prior_reviewer:
        raise ObsError(
            f"request was previously declined and {user} was not a prior "
            f"reviewer; refusing to assign (a re-review needs the original "
            f"reviewer)"
        )


def assign(
    client: ObsClient,
    config: Config,
    rrid: RequestReviewID,
    user: str,
    groups: list[str],
) -> None:
    """Assign the review to ``user`` for the resolved group(s)."""
    request = _get_request(client, rrid)
    if request.state != "review":
        raise ObsError(
            f"request {request.reqid} is not open for review "
            f"(state={request.state!r}); refusing to assign"
        )
    resolved = _resolve_assign_groups(client, request, user, groups)
    if not _is_slfo(rrid):
        if preconditions.fetch_testreport_log(config, rrid) is None:
            raise ObsError(
                f"no testreport found for {rrid} on qam.suse.de; refusing to "
                "assign (the report generator may still be running)"
            )
        _check_previous_rejects(client, request, user)
    for group in resolved:
        client.post(
            f"request/{_reqid(rrid)}",
            params={"cmd": "assignreview", "reviewer": user, "by_group": group},
            body=_ASSIGN_MSG.format(user=user, group=group, request=rrid),
        )


# --------------------------------------------------------------------------- #
# unassign                                                                     #
# --------------------------------------------------------------------------- #
def unassign(
    client: ObsClient,
    config: Config,
    rrid: RequestReviewID,
    user: str,
    groups: list[str],
) -> None:
    """Revert ``user``'s assignment for the resolved (or explicit) group(s)."""
    request = _get_request(client, rrid)
    own = assignments_for_user(request, user)
    if not own:
        raise ObsError(
            f"{user} holds no review assignment on request {request.reqid}; "
            "nothing to unassign"
        )
    resolved = groups or sorted({a.group for a in own})
    for group in resolved:
        client.post(
            f"request/{_reqid(rrid)}",
            params={
                "cmd": "assignreview",
                "revert": 1,
                "reviewer": user,
                "by_group": group,
            },
            body=_UNASSIGN_MSG.format(user=user, group=group, request=rrid),
        )


# --------------------------------------------------------------------------- #
# approve (user path only)                                                     #
# --------------------------------------------------------------------------- #
def approve(
    client: ObsClient,
    config: Config,
    rrid: RequestReviewID,
    user: str,
    groups: list[str],
) -> None:
    """Accept the review by user; group-approve is refused (parity)."""
    if groups:
        raise ObsError(
            "group approval is not supported by the native OBS backend "
            "(it can leave the update in an inconsistent state); approve the "
            "review assigned to you without -g"
        )
    request = _get_request(client, rrid)
    if not assignments_for_user(request, user):
        raise ObsError(
            f"{user} is not assigned to request {request.reqid}; assign it to "
            "yourself before approving"
        )
    if not _is_slfo(rrid):
        log = preconditions.fetch_testreport_log(config, rrid)
        if log is None or preconditions.summary(log) != "PASSED":
            raise ObsError(f"testreport for {rrid} is not PASSED; refusing to approve")
    comment = _PREFIX + _APPROVE_MSG.format(
        request=rrid, user=user, url=_fancy_url(config, rrid)
    )
    _changereviewstate(client, rrid, "accepted", user, comment)


# --------------------------------------------------------------------------- #
# reject (always by_user)                                                      #
# --------------------------------------------------------------------------- #
def _write_reject_reason(
    client: ObsClient, request: models.Request, rrid: RequestReviewID, reason: str
) -> None:
    """Append ``<reqid>:<reason>`` to the source project's MAINT:RejectReason."""
    if request.src_project is None:
        return
    path = f"source/{request.src_project}/_attribute/{models.REJECT_REASON_NAMESPACE}:{models.REJECT_REASON_NAME}"
    existing = models.parse_reject_reason_values(client.get(path).text)
    merged = [*existing, f"{_reqid(rrid)}:{reason}"]
    client.post(path, body=models.build_reject_reason_body(merged))


def reject(
    client: ObsClient,
    config: Config,
    rrid: RequestReviewID,
    user: str,
    groups: list[str],
    reason: str,
    message: str,
) -> None:
    """Decline the review by user, recording the reject reason attribute."""
    if groups:
        logger.info("reject ignores -g/--group (native reject is always by_user)")
    request = _get_request(client, rrid)
    if not _is_slfo(rrid):
        log = preconditions.fetch_testreport_log(config, rrid)
        if log is None or preconditions.summary(log) != "FAILED":
            raise ObsError(f"testreport for {rrid} is not FAILED; refusing to reject")
        if not preconditions.comment(log):
            raise ObsError(f"testreport for {rrid} has no comment; refusing to reject")
        _write_reject_reason(client, request, rrid, reason)
    # Parity: the reviewer's -M message is not recorded in the decline comment.
    comment = _PREFIX + _DECLINE_MSG.format(
        request=rrid, user=user, url=_fancy_url(config, rrid)
    )
    _changereviewstate(client, rrid, "declined", user, comment)
