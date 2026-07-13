"""Tests for the native dispatch of the OSC facade (mtui.data_sources.oscqam)."""

import xml.etree.ElementTree as ET
from pathlib import Path
from types import SimpleNamespace

import pytest
import requests

from mtui.data_sources import oscqam
from mtui.data_sources.obs.oscrc import ObsCredentials
from mtui.data_sources.oscqam import OSC
from mtui.support.exceptions import ObsConfigError, ObsError
from mtui.types.rrid import RequestReviewID

RRID = RequestReviewID("SUSE:Maintenance:1:56789")
CREDS = ObsCredentials(
    apiurl="https://api.suse.de", user="qamuser", sshkey_path=Path("/x"), source="s"
)


def _config():
    return SimpleNamespace(
        obs_api_url="https://api.suse.de",
        obs_conffile="",
        obs_request_timeout=180,
        ssl_verify=True,
        reports_url="https://qam.suse.de/testreports",
        fancy_reports_url="https://qam.suse.de/reports",
    )


@pytest.fixture
def wired(monkeypatch):
    """Stub credential + client construction; record native op calls."""
    monkeypatch.setattr(oscqam, "read_credentials", lambda *a, **k: CREDS)
    sentinel_client = object()
    monkeypatch.setattr(oscqam, "ObsClient", lambda *a, **k: sentinel_client)
    calls: dict[str, tuple] = {}

    def recorder(name):
        def _rec(*args):
            calls[name] = args

        return _rec

    for name in ("approve", "assign", "unassign", "reject", "comment"):
        monkeypatch.setattr(oscqam.obs_qam, name, recorder(name))
    return calls, sentinel_client


def test_native_approve_dispatches(wired):
    calls, client = wired
    assert OSC(_config(), RRID).approve(["qam-sle"]) is True
    c, _cfg, rrid, user, groups = calls["approve"]
    assert (c, rrid, user, groups) == (client, RRID, "qamuser", ["qam-sle"])


def test_native_group_none_normalised_to_empty_list(wired):
    calls, _ = wired
    OSC(_config(), RRID).assign(None)  # ty: ignore[invalid-argument-type]
    assert calls["assign"][-1] == []


def test_native_comment_and_reject_signatures(wired):
    calls, client = wired
    OSC(_config(), RRID).comment("hi")
    assert calls["comment"] == (client, RRID, "hi")
    OSC(_config(), RRID).reject(["g"], "not_fixed", "msg")
    c, _cfg, rrid, user, groups, reason, message = calls["reject"]
    assert (reason, message, groups) == ("not_fixed", "msg", ["g"])


@pytest.mark.parametrize(
    "exc",
    [
        ObsError("boom"),
        ObsConfigError("no oscrc"),
        requests.exceptions.ConnectionError("down"),
        ET.ParseError("bad xml"),
    ],
)
def test_native_never_raises_returns_false(monkeypatch, exc):
    monkeypatch.setattr(oscqam, "read_credentials", lambda *a, **k: CREDS)
    monkeypatch.setattr(oscqam, "ObsClient", lambda *a, **k: object())

    def boom(*a):
        raise exc

    monkeypatch.setattr(oscqam.obs_qam, "comment", boom)
    assert OSC(_config(), RRID).comment("x") is False


def test_native_credential_error_returns_false(monkeypatch):
    def boom(*a, **k):
        raise ObsConfigError("no section")

    monkeypatch.setattr(oscqam, "read_credentials", boom)
    assert OSC(_config(), RRID).assign(["g"]) is False
