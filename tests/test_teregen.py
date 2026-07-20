"""Tests for the read-only TeReGen Report API client.

The command-level tests mock the ``TeReGen`` client wholesale, so the client's
own request/parse/poll logic is exercised here directly against ``responses``-
mocked HTTP. Every method is best-effort: a transport or decode failure must
degrade to ``None`` rather than raise.
"""

from __future__ import annotations

import requests
import responses

from mtui.data_sources.teregen import RegenOutcome, TeReGen

API = "https://qam.suse.de/api/v1"
RRID = "SUSE:SLFO:1.2:5702"


def _client(mock_config) -> TeReGen:
    mock_config.teregen_api = API
    return TeReGen(mock_config)


@responses.activate
def test_get_returns_decoded_json(mock_config):
    responses.add(responses.GET, f"{API}/reports/{RRID}", json={"id": RRID}, status=200)
    assert _client(mock_config).info(RRID) == {"id": RRID}


@responses.activate
def test_get_swallows_http_error(mock_config):
    responses.add(responses.GET, f"{API}/reports/{RRID}", status=500)
    assert _client(mock_config).info(RRID) is None


@responses.activate
def test_get_swallows_invalid_json(mock_config):
    responses.add(responses.GET, f"{API}/reports/{RRID}", body="not json", status=200)
    assert _client(mock_config).info(RRID) is None


@responses.activate
def test_info_rejects_non_dict_payload(mock_config):
    responses.add(responses.GET, f"{API}/reports/{RRID}", json=["x"], status=200)
    assert _client(mock_config).info(RRID) is None


@responses.activate
def test_metadata_and_status(mock_config):
    responses.add(
        responses.GET, f"{API}/reports/{RRID}/metadata", json={"rating": "low"}
    )
    responses.add(
        responses.GET,
        f"{API}/reports/{RRID}/status",
        json={"template": True, "minion_state": "finished"},
    )
    c = _client(mock_config)
    assert c.metadata(RRID) == {"rating": "low"}
    assert c.status(RRID) == {"template": True, "minion_state": "finished"}


@responses.activate
def test_checkers_unwraps_list(mock_config):
    responses.add(
        responses.GET,
        f"{API}/reports/{RRID}/checkers",
        json={"checkers": [{"name": "rpmlint"}]},
    )
    assert _client(mock_config).checkers(RRID) == [{"name": "rpmlint"}]


@responses.activate
def test_checkers_none_on_failure(mock_config):
    responses.add(responses.GET, f"{API}/reports/{RRID}/checkers", status=503)
    assert _client(mock_config).checkers(RRID) is None


@responses.activate
def test_updates_without_filters(mock_config):
    responses.add(responses.GET, f"{API}/updates", json={"updates": [1, 2]})
    assert _client(mock_config).updates() == [1, 2]


@responses.activate
def test_updates_passes_query_filters(mock_config):
    responses.add(responses.GET, f"{API}/updates", json={"updates": []})
    assert _client(mock_config).updates(review_group="qam-sle", status="testing") == []
    qs = responses.calls[0].request.url or ""
    assert "review_group=qam-sle" in qs
    assert "status=testing" in qs


@responses.activate
def test_updates_passes_assignee(mock_config):
    responses.add(responses.GET, f"{API}/updates", json={"updates": []})
    assert _client(mock_config).updates(assignee="mpluskal", status="testing") == []
    qs = responses.calls[0].request.url or ""
    assert "assignee=mpluskal" in qs
    assert "status=testing" in qs


@responses.activate
def test_updates_passes_unassigned_flag(mock_config):
    responses.add(responses.GET, f"{API}/updates", json={"updates": []})
    _client(mock_config).updates(unassigned=True)
    qs = responses.calls[0].request.url or ""
    assert "unassigned=1" in qs


@responses.activate
def test_updates_passes_with_assignment_flag(mock_config):
    responses.add(responses.GET, f"{API}/updates", json={"updates": []})
    _client(mock_config).updates(with_assignment=True)
    qs = responses.calls[0].request.url or ""
    assert "with_assignment=1" in qs


@responses.activate
def test_updates_passes_no_cache_flag(mock_config):
    responses.add(responses.GET, f"{API}/updates", json={"updates": []})
    _client(mock_config).updates(assignee="mpluskal", no_cache=True)
    qs = responses.calls[0].request.url or ""
    assert "no_cache=1" in qs
    assert "assignee=mpluskal" in qs


@responses.activate
def test_updates_omits_unset_assignment_flags(mock_config):
    responses.add(responses.GET, f"{API}/updates", json={"updates": []})
    _client(mock_config).updates()
    url = responses.calls[0].request.url or ""
    assert "unassigned" not in url
    assert "with_assignment" not in url
    assert "no_cache" not in url
    assert "assignee" not in url


@responses.activate
def test_regenerate_returns_json_body(mock_config):
    responses.add(
        responses.POST,
        f"{API}/reports/{RRID}/regenerate",
        json={"id": RRID, "job": 42},
        status=202,
    )
    result = _client(mock_config).regenerate(RRID, force_overwrite=True)
    assert result == {"id": RRID, "job": 42}
    assert responses.calls[0].request.body == (
        b'{"force_overwrite": true, "ignore_inconsistent": false}'
    )


@responses.activate
def test_regenerate_surfaces_refusal_body(mock_config):
    responses.add(
        responses.POST,
        f"{API}/reports/{RRID}/regenerate",
        json={"error": "template was hand-edited"},
        status=409,
    )
    assert _client(mock_config).regenerate(RRID) == {
        "error": "template was hand-edited"
    }


@responses.activate
def test_regenerate_accepts_empty_202(mock_config):
    responses.add(
        responses.POST, f"{API}/reports/{RRID}/regenerate", body="", status=202
    )
    assert _client(mock_config).regenerate(RRID) == {}


@responses.activate
def test_regenerate_non_json_error_maps_to_http_status(mock_config):
    responses.add(
        responses.POST, f"{API}/reports/{RRID}/regenerate", body="oops", status=400
    )
    assert _client(mock_config).regenerate(RRID) == {"error": "HTTP 400"}


@responses.activate
def test_regenerate_none_when_unreachable(mock_config):
    responses.add(
        responses.POST,
        f"{API}/reports/{RRID}/regenerate",
        body=requests.exceptions.ConnectionError("boom"),
    )
    assert _client(mock_config).regenerate(RRID) is None


@responses.activate
def test_wait_for_template_returns_on_finished(mock_config):
    responses.add(
        responses.GET,
        f"{API}/reports/{RRID}/status",
        json={"minion_state": "finished"},
    )
    status = _client(mock_config).wait_for_template(RRID)
    assert status == {"minion_state": "finished"}


@responses.activate
def test_wait_for_template_polls_until_done(mock_config, monkeypatch):
    # The inter-poll sleep is an Event.wait; stub it so the test never blocks.
    monkeypatch.setattr(
        "mtui.data_sources.teregen.threading.Event.wait",
        lambda self, timeout=None: None,
    )
    responses.add(
        responses.GET, f"{API}/reports/{RRID}/status", json={"minion_state": "running"}
    )
    responses.add(
        responses.GET, f"{API}/reports/{RRID}/status", json={"minion_state": "failed"}
    )
    status = _client(mock_config).wait_for_template(RRID, interval=0.01)
    assert status == {"minion_state": "failed"}


@responses.activate
def test_wait_for_template_returns_last_on_timeout(mock_config):
    responses.add(
        responses.GET, f"{API}/reports/{RRID}/status", json={"minion_state": "running"}
    )
    # timeout=0 -> the deadline is already reached after the first poll, so the
    # last seen ("running") status is returned without sleeping.
    status = _client(mock_config).wait_for_template(RRID, timeout=0)
    assert status == {"minion_state": "running"}


@responses.activate
def test_wait_for_template_stops_when_should_stop_true(mock_config):
    # should_stop already True -> the wait returns the first-seen status after a
    # single poll, without sleeping or polling again.
    responses.add(
        responses.GET, f"{API}/reports/{RRID}/status", json={"minion_state": "running"}
    )
    status = _client(mock_config).wait_for_template(
        RRID, interval=999, timeout=999, should_stop=lambda: True
    )
    assert status == {"minion_state": "running"}
    assert len(responses.calls) == 1


@responses.activate
def test_wait_for_template_should_stop_sleep_is_interruptible(mock_config):
    # The interruptible-sleep loop must not block on the full interval: it polls
    # should_stop in small steps. Here should_stop flips to True after the first
    # poll, so the wait returns promptly with the last status.
    responses.add(
        responses.GET, f"{API}/reports/{RRID}/status", json={"minion_state": "running"}
    )
    calls = {"n": 0}

    def should_stop() -> bool:
        # Stay False for the post-poll check, then flip True so the small-step
        # sleep loop exits on its first iteration instead of waiting 10s.
        calls["n"] += 1
        return calls["n"] > 1

    status = _client(mock_config).wait_for_template(
        RRID, interval=10, timeout=999, should_stop=should_stop
    )
    assert status == {"minion_state": "running"}


@responses.activate
def test_updates_encodes_filter_params(mock_config):
    responses.add(responses.GET, f"{API}/updates", json={"updates": []})
    _client(mock_config).updates(review_group="qam sle&x", status="testing")
    # Values with spaces / '&' must be URL-encoded by requests, not concatenated
    # raw into the query string.
    url = responses.calls[0].request.url
    assert url is not None
    qs = url.split("?", 1)[1]
    assert "review_group=qam+sle%26x" in qs or "review_group=qam%20sle%26x" in qs
    assert "status=testing" in qs


@responses.activate
def test_updates_no_params_omits_query(mock_config):
    responses.add(responses.GET, f"{API}/updates", json={"updates": []})
    _client(mock_config).updates()
    url = responses.calls[0].request.url
    assert url is not None
    assert "?" not in url


@responses.activate
def test_regenerate_and_wait_ok(mock_config):
    responses.add(
        responses.POST,
        f"{API}/reports/{RRID}/regenerate",
        json={"id": RRID, "job": 5},
        status=202,
    )
    responses.add(
        responses.GET, f"{API}/reports/{RRID}/status", json={"minion_state": "finished"}
    )
    outcome = _client(mock_config).regenerate_and_wait(RRID)
    assert outcome == RegenOutcome(ok=True, job=5)


@responses.activate
def test_regenerate_and_wait_unreachable(mock_config):
    responses.add(
        responses.POST,
        f"{API}/reports/{RRID}/regenerate",
        body=requests.exceptions.ConnectionError("boom"),
    )
    outcome = _client(mock_config).regenerate_and_wait(RRID)
    assert outcome.unreachable is True
    assert outcome.ok is False


@responses.activate
def test_regenerate_and_wait_refused(mock_config):
    responses.add(
        responses.POST,
        f"{API}/reports/{RRID}/regenerate",
        json={"error": "edited"},
        status=409,
    )
    outcome = _client(mock_config).regenerate_and_wait(RRID)
    assert outcome == RegenOutcome(ok=False, error="edited")


@responses.activate
def test_regenerate_and_wait_unfinished(mock_config):
    responses.add(
        responses.POST,
        f"{API}/reports/{RRID}/regenerate",
        json={"id": RRID, "job": 8},
        status=202,
    )
    responses.add(
        responses.GET,
        f"{API}/reports/{RRID}/status",
        json={"minion_state": "failed", "minion_error": "kaboom"},
    )
    outcome = _client(mock_config).regenerate_and_wait(RRID)
    assert outcome == RegenOutcome(
        ok=False, state="failed", minion_error="kaboom", job=8
    )


# --- granular parsed endpoints (teregen b22f755) ---


@responses.activate
def test_parsed_full_report(mock_config):
    body = {"sections": [], "summary": {}, "completeness": {"complete": True}}
    responses.add(responses.GET, f"{API}/reports/{RRID}/parsed", json=body, status=200)
    assert _client(mock_config).parsed(RRID) == body


@responses.activate
def test_parsed_section_slice(mock_config):
    body = {"id": RRID, "section": "metadata", "data": {"rating": "important"}}
    responses.add(
        responses.GET,
        f"{API}/reports/{RRID}/parsed/metadata",
        json=body,
        status=200,
    )
    # The server wraps a section in an {id, section, data} envelope.
    assert _client(mock_config).parsed(RRID, "metadata") == body


@responses.activate
def test_bugs_index(mock_config):
    body = {"bugs": [{"id": "bsc#1", "status": "NEW", "is_new": True}]}
    responses.add(responses.GET, f"{API}/reports/{RRID}/bugs", json=body, status=200)
    assert _client(mock_config).bugs(RRID) == body


@responses.activate
def test_bugs_single_id_is_percent_encoded(mock_config):
    """'bsc#1196693' must reach the server as bugs/bsc%231196693.

    An unencoded '#' would be a fragment separator and truncate the path.
    """
    body = {"id": RRID, "bug_id": "bsc#1196693", "bug": {"status": "RESOLVED"}}
    responses.add(
        responses.GET,
        f"{API}/reports/{RRID}/bugs/bsc%231196693",
        json=body,
        status=200,
    )
    out = _client(mock_config).bugs(RRID, "bsc#1196693")
    assert out == body
    assert (responses.calls[0].request.url or "").endswith("/bugs/bsc%231196693")


@responses.activate
def test_completeness(mock_config):
    body = {
        "id": RRID,
        "complete": False,
        "unfilled": [{"field": "put here", "value": ""}],
    }
    responses.add(
        responses.GET,
        f"{API}/reports/{RRID}/completeness",
        json=body,
        status=200,
    )
    assert _client(mock_config).completeness(RRID) == body


@responses.activate
def test_parsed_non_dict_payload_is_none(mock_config):
    responses.add(responses.GET, f"{API}/reports/{RRID}/parsed", json=["x"], status=200)
    assert _client(mock_config).parsed(RRID) is None
