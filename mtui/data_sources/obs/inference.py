"""Assignment/role inference — the plugin's exact state machine.

Ported verbatim from openSUSE/osc-plugin-qam
(``oscqam/models/assignment.py`` ``Assignment.infer``/``infer_group``,
GPL-2.0-only, same licence as mtui). This single source of truth backs BOTH
``unassign``'s "the user holds >=1 assignment" guard and ``approve``'s
"the user is assigned" role check, so a merely-*assigned*-but-not-*accepted*
user does not count and a finished reviewer is dropped — matching the plugin.

The machine replays each qam group review's NESTED history in ``when`` order:
add an assignment on "Review got accepted", remove it on "Review got
reopened", ignore "Review got assigned"; then drop every assignment whose
user already has an accepted ``by_user`` review (a finished reviewer).
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import UTC, datetime

from .models import Request, Review, is_qam_group

_ASSIGNED = "Review got assigned"
_ACCEPTED = "Review got accepted"
_REOPENED = "Review got reopened"
_RELEVANT = frozenset({_ASSIGNED, _ACCEPTED, _REOPENED})


@dataclass(frozen=True, slots=True)
class Assignment:
    """A resolved "``user`` reviews for ``group``" pairing."""

    user: str
    group: str


def _when_key(when: str) -> tuple[int, datetime]:
    """A chronological sort key for a history ``when`` string.

    Lenient: parses ISO-8601 (with or without a ``Z``/offset), normalises to
    naive UTC so all keys compare, and sorts unparseable values last.
    """
    try:
        parsed = datetime.fromisoformat(when.strip().replace("Z", "+00:00"))
    except ValueError:
        return (1, datetime.max)
    if parsed.tzinfo is not None:
        parsed = parsed.astimezone(UTC).replace(tzinfo=None)
    return (0, parsed)


def _infer_group(review: Review, group: str) -> set[Assignment]:
    events = sorted(
        (e for e in review.history if e.description in _RELEVANT),
        key=lambda e: _when_key(e.when),
    )
    assignments: set[Assignment] = set()
    for event in events:
        if event.description == _ACCEPTED:
            assignments.add(Assignment(event.who, group))
        elif event.description == _REOPENED:
            assignments.discard(Assignment(event.who, group))
        # _ASSIGNED is a no-op (a group review being picked up is not yet a
        # completed assignment).
    return assignments


def infer(request: Request) -> set[Assignment]:
    """Resolve the full set of active user->group assignments for a request."""
    assignments: set[Assignment] = set()
    for review in request.reviews:
        group = review.by_group
        if group and is_qam_group(group) and review.state in ("accepted", "new"):
            assignments |= _infer_group(review, group)

    finished_users = {
        review.by_user
        for review in request.reviews
        if review.by_user and review.state == "accepted"
    }
    return {a for a in assignments if a.user not in finished_users}


def assignments_for_user(request: Request, user: str) -> set[Assignment]:
    """The subset of :func:`infer` assignments belonging to ``user``."""
    return {a for a in infer(request) if a.user == user}
