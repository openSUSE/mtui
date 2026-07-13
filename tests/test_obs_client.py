"""Tests for the OBS HTTP client (mtui.data_sources.obs.client)."""

import time
from pathlib import Path
from types import SimpleNamespace

import pytest
import requests
import responses

from mtui.data_sources.obs.client import ObsClient, _error_summary
from mtui.data_sources.obs.errors import ObsApiError, ObsTimeoutError
from mtui.data_sources.obs.oscrc import ObsCredentials

API = "https://api.suse.de"


def _config():
    return SimpleNamespace(obs_api_url=API, obs_request_timeout=180, ssl_verify=True)


def _creds():
    return ObsCredentials(
        apiurl=API, user="qamuser", sshkey_path=Path("/nonexistent"), source="x"
    )


@pytest.fixture
def client():
    return ObsClient(_config(), _creds())


@responses.activate
def test_get_success(client):
    responses.add(responses.GET, f"{API}/request/1", status=200, body="<request/>")
    resp = client.get("request/1", params={"withfullhistory": 1})
    assert resp.status_code == 200
    assert resp.text == "<request/>"
    assert responses.calls[0].request.headers["Accept"] == "application/xml"


@responses.activate
def test_post_sends_xml_body(client):
    responses.add(responses.POST, f"{API}/comments/request/1", status=200, body="<ok/>")
    client.post("comments/request/1", body="a comment")
    req = responses.calls[0].request
    assert req.body == b"a comment"
    assert req.headers["Content-Type"] == "application/xml; charset=utf-8"
    assert req.headers["Accept"] == "application/xml"


@responses.activate
def test_non_2xx_raises_obs_api_error_with_summary(client):
    responses.add(
        responses.GET,
        f"{API}/request/9",
        status=404,
        body='<status code="not_found"><summary>Request 9 not found</summary></status>',
    )
    with pytest.raises(ObsApiError) as ei:
        client.get("request/9")
    assert ei.value.status == 404
    assert ei.value.summary == "Request 9 not found"


@responses.activate
def test_non_2xx_non_xml_body_has_empty_summary(client):
    responses.add(responses.GET, f"{API}/x", status=500, body="Internal Error")
    with pytest.raises(ObsApiError) as ei:
        client.get("x")
    assert ei.value.summary == ""


@responses.activate
def test_tls_error_logs_hint_and_raises(client, caplog):
    responses.add(
        responses.GET, f"{API}/x", body=requests.exceptions.SSLError("cert boom")
    )
    with (
        caplog.at_level("ERROR"),
        pytest.raises(requests.exceptions.RequestException),
    ):
        client.get("x")
    assert any(
        "certificate" in r.message.lower() or "CA" in r.message for r in caplog.records
    )


@responses.activate
def test_non_tls_request_error_raises(client, caplog):
    responses.add(
        responses.GET, f"{API}/x", body=requests.exceptions.ConnectionError("down")
    )
    with caplog.at_level("ERROR"), pytest.raises(requests.exceptions.RequestException):
        client.get("x")
    assert any("failed" in r.message for r in caplog.records)


def test_between_calls_budget_aborts(client):
    """An exhausted wall-clock budget aborts the next call (no mid-call kill)."""
    client._deadline = time.monotonic() - 1
    with pytest.raises(ObsTimeoutError):
        client.get("request/1")


@pytest.mark.parametrize(
    ("body", "expected"),
    [
        ("<status><summary> boom </summary></status>", "boom"),
        ("<status/>", ""),
        ("not xml", ""),
    ],
)
def test_error_summary(body, expected):
    assert _error_summary(body) == expected
