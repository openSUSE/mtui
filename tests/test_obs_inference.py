"""Tests for the assignment state machine (mtui.data_sources.obs.inference).

These pin the exact plugin semantics: an assignment appears only on
"Review got accepted", is removed on "Review got reopened", a mere
"Review got assigned" does not count, and a finished (accepted by_user)
reviewer is dropped.
"""

from mtui.data_sources.obs import inference
from mtui.data_sources.obs.inference import Assignment
from mtui.data_sources.obs.models import parse_request


def _request(*reviews: str) -> str:
    return f"<request id='1'><state name='review'/>{''.join(reviews)}</request>"


def _group_review(group: str, state: str, *events: tuple[str, str, str]) -> str:
    history = "".join(
        f"<history who='{who}' when='{when}'><description>{desc}</description></history>"
        for who, when, desc in events
    )
    return f"<review state='{state}' by_group='{group}'>{history}</review>"


ACCEPT = "Review got accepted"
ASSIGN = "Review got assigned"
REOPEN = "Review got reopened"


def test_accepted_history_yields_assignment():
    req = parse_request(
        _request(
            _group_review(
                "qam-sle", "accepted", ("alice", "2017-01-01T00:00:00", ACCEPT)
            )
        )
    )
    assert inference.infer(req) == {Assignment("alice", "qam-sle")}


def test_assigned_only_does_not_count():
    """A group review that was only *assigned* (not accepted) is not an assignment."""
    req = parse_request(
        _request(
            _group_review("qam-sle", "new", ("alice", "2017-01-01T00:00:00", ASSIGN))
        )
    )
    assert inference.infer(req) == set()


def test_reopened_after_accepted_removes_assignment():
    req = parse_request(
        _request(
            _group_review(
                "qam-sle",
                "new",
                ("alice", "2017-01-01T00:00:00", ACCEPT),
                ("alice", "2017-01-02T00:00:00", REOPEN),
            )
        )
    )
    assert inference.infer(req) == set()


def test_out_of_order_history_is_sorted_by_when():
    """Events replay in `when` order regardless of document order."""
    req = parse_request(
        _request(
            _group_review(
                "qam-sle",
                "new",
                ("alice", "2017-01-02T00:00:00", REOPEN),  # later, listed first
                ("alice", "2017-01-01T00:00:00", ACCEPT),  # earlier
            )
        )
    )
    # Chronologically: accepted then reopened -> no assignment.
    assert inference.infer(req) == set()


def test_finished_user_review_drops_assignment():
    req = parse_request(
        _request(
            _group_review(
                "qam-sle", "accepted", ("alice", "2017-01-01T00:00:00", ACCEPT)
            ),
            "<review state='accepted' by_user='alice'/>",
        )
    )
    assert inference.infer(req) == set()


def test_automation_groups_are_ignored():
    req = parse_request(
        _request(
            _group_review(
                "qam-auto", "accepted", ("bot", "2017-01-01T00:00:00", ACCEPT)
            ),
            _group_review(
                "qam-openqa", "accepted", ("bot", "2017-01-01T00:00:00", ACCEPT)
            ),
        )
    )
    assert inference.infer(req) == set()


def test_assignments_for_user_filters():
    req = parse_request(
        _request(
            _group_review(
                "qam-sle", "accepted", ("alice", "2017-01-01T00:00:00", ACCEPT)
            ),
            _group_review(
                "qam-cloud", "accepted", ("bob", "2017-01-01T00:00:00", ACCEPT)
            ),
        )
    )
    assert inference.assignments_for_user(req, "alice") == {
        Assignment("alice", "qam-sle")
    }


def test_timezone_aware_when_is_normalised():
    """A ``Z``/offset timestamp is parsed and ordered against naive ones."""
    req = parse_request(
        _request(
            _group_review(
                "qam-sle",
                "new",
                ("alice", "2017-01-01T00:00:00Z", ACCEPT),
                ("alice", "2017-01-02T00:00:00+00:00", REOPEN),
            )
        )
    )
    assert inference.infer(req) == set()


def test_unparseable_when_sorts_last_without_crashing():
    req = parse_request(
        _request(
            _group_review(
                "qam-sle",
                "new",
                ("alice", "not-a-date", ACCEPT),
                ("alice", "2017-01-01T00:00:00", ACCEPT),
            )
        )
    )
    assert inference.infer(req) == {Assignment("alice", "qam-sle")}
