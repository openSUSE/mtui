"""Tests for :mod:`mtui.data_sources.smelt`."""

from __future__ import annotations

from types import SimpleNamespace
from typing import cast

import responses

from mtui.data_sources.smelt import Smelt, slfo_update_id
from mtui.support.config import Config
from mtui.types import RequestReviewID

SMELT = "https://smelt.example.com"
V2 = f"{SMELT}/api/experimental/v2"


def _cfg(url: str = SMELT) -> Config:
    """Minimal stand-in for Config — Smelt only reads smelt_url + ssl_verify."""
    return cast("Config", SimpleNamespace(smelt_url=url, ssl_verify=True))


def test_slfo_update_id_builds_from_host():
    assert (
        slfo_update_id("https://src.suse.de/products/SLFO/pulls/5137", 5137)
        == "src.suse.de:products:SLFO:5137"
    )
    assert slfo_update_id("", 5137) is None


def test_not_configured_is_inert():
    s = Smelt(_cfg(""))
    assert s.configured is False
    assert s.update("x") is None
    assert s.checker_results("x") == []
    assert s.priority_deadline(RequestReviewID("SUSE:SLFO:1.2:5137")) == (None, None)


@responses.activate
def test_update_returns_detail():
    responses.add(
        responses.GET,
        f"{V2}/updates/src.suse.de:products:SLFO:5137",
        json={"status": "success", "data": {"priority": 637, "deadline": "2026-07-06"}},
        status=200,
    )
    s = Smelt(_cfg())
    d = s.update("src.suse.de:products:SLFO:5137")
    assert d is not None
    assert d["priority"] == 637


@responses.activate
def test_checker_results_unwraps_list():
    responses.add(
        responses.GET,
        f"{V2}/updates/u/checker-results",
        json={
            "status": "success",
            "data": [{"checker_type": "staging", "fail_count": 0}],
        },
        status=200,
    )
    rows = Smelt(_cfg()).checker_results("u")
    assert rows[0]["checker_type"] == "staging"


@responses.activate
def test_unreleased_passes_review_group():
    responses.add(
        responses.GET,
        f"{V2}/updates/unreleased",
        json={
            "status": "success",
            "data": [{"human_readable_id": "products/SLFO #5137"}],
        },
        status=200,
    )
    rows = Smelt(_cfg()).unreleased(review_group="qam-sle-review")
    assert len(rows) == 1
    assert "review_group=qam-sle-review" in (responses.calls[0].request.url or "")


@responses.activate
def test_priority_deadline_slfo_via_rest():
    responses.add(
        responses.GET,
        f"{V2}/updates/src.suse.de:products:SLFO:5137",
        json={"status": "success", "data": {"priority": 900, "deadline": "2026-07-01"}},
        status=200,
    )
    s = Smelt(_cfg())
    prio, deadline = s.priority_deadline(
        RequestReviewID("SUSE:SLFO:1.2:5137"),
        "https://src.suse.de/products/SLFO/pulls/5137",
    )
    assert (prio, deadline) == (900, "2026-07-01")


@responses.activate
def test_priority_deadline_maintenance_via_graphql():
    responses.add(
        responses.POST,
        f"{SMELT}/graphql/",
        json={
            "data": {
                "incidents": {
                    "edges": [{"node": {"priority": 42, "crd": "2026-08-01"}}]
                }
            }
        },
        status=200,
    )
    s = Smelt(_cfg())
    prio, deadline = s.priority_deadline(RequestReviewID("SUSE:Maintenance:1234:5678"))
    assert (prio, deadline) == (42, "2026-08-01")


@responses.activate
def test_priority_deadline_maintenance_falls_back_to_prd():
    """``crd`` is null on most incidents; the deadline then comes from ``prd``
    (the planned release date SMELT shows), not ``?``.
    """
    responses.add(
        responses.POST,
        f"{SMELT}/graphql/",
        json={
            "data": {
                "incidents": {
                    "edges": [
                        {"node": {"priority": 357, "crd": None, "prd": "2026-07-06"}}
                    ]
                }
            }
        },
        status=200,
    )
    s = Smelt(_cfg())
    prio, deadline = s.priority_deadline(
        RequestReviewID("SUSE:Maintenance:44997:414920")
    )
    assert (prio, deadline) == (357, "2026-07-06")


@responses.activate
def test_priority_deadline_maintenance_prefers_crd_over_prd():
    """A set customer-required date takes precedence over the planned date."""
    responses.add(
        responses.POST,
        f"{SMELT}/graphql/",
        json={
            "data": {
                "incidents": {
                    "edges": [
                        {
                            "node": {
                                "priority": 9,
                                "crd": "2026-08-01",
                                "prd": "2026-09-01",
                            }
                        }
                    ]
                }
            }
        },
        status=200,
    )
    s = Smelt(_cfg())
    prio, deadline = s.priority_deadline(RequestReviewID("SUSE:Maintenance:1:2"))
    assert (prio, deadline) == (9, "2026-08-01")


@responses.activate
def test_v2_transport_error_returns_none():
    responses.add(responses.GET, f"{V2}/updates/x", status=500)
    assert Smelt(_cfg()).update("x") is None


@responses.activate
def test_requests_returns_nodes():
    responses.add(
        responses.POST,
        f"{SMELT}/graphql/",
        json={
            "data": {
                "requests": {
                    "edges": [
                        {
                            "node": {
                                "requestId": 414206,
                                "incident": {"incidentId": 44861},
                            }
                        }
                    ]
                }
            }
        },
        status=200,
    )
    nodes = Smelt(_cfg()).review_requests(group="qam-sle", status="review")
    assert nodes[0]["requestId"] == 414206
