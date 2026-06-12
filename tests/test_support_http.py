"""Tests for the centralized HTTP timeout / TLS-verification helper."""

import pytest

from mtui.support import http as _http
from mtui.support.config import _parse_ssl_verify


@pytest.fixture(autouse=True)
def _reset_warning_flag():
    """Reset the module-level idempotency flag around each test."""
    original = _http._warnings_disabled
    _http._warnings_disabled = False
    yield
    _http._warnings_disabled = original


def test_http_timeout_is_positive_connect_read_tuple():
    connect, read = _http.HTTP_TIMEOUT
    assert connect > 0
    assert read > 0


@pytest.mark.parametrize(
    ("default", "override", "expected"),
    [
        (False, None, False),  # unset override -> keep per-site default
        (True, None, True),
        (False, True, True),  # override wins over default
        (True, False, False),
        (False, "/etc/ssl/ca.pem", "/etc/ssl/ca.pem"),  # CA bundle path
    ],
)
def test_resolve_verify_precedence(default, override, expected):
    assert _http.resolve_verify(default, override) == expected


def test_build_session_sets_verify_true_and_keeps_warnings(monkeypatch):
    calls = []
    monkeypatch.setattr(
        _http.urllib3, "disable_warnings", lambda *a, **k: calls.append(a)
    )

    session = _http.build_session(verify=True)

    assert session.verify is True
    # Verification is on, so the insecure-request warning must stay active.
    assert calls == []
    assert _http._warnings_disabled is False


def test_build_session_disables_warnings_when_verify_off(monkeypatch):
    calls = []
    monkeypatch.setattr(
        _http.urllib3, "disable_warnings", lambda *a, **k: calls.append(a)
    )

    session = _http.build_session(verify=False)

    assert session.verify is False
    assert len(calls) == 1
    assert _http._warnings_disabled is True


def test_build_session_with_ca_bundle_path_keeps_warnings(monkeypatch):
    calls = []
    monkeypatch.setattr(
        _http.urllib3, "disable_warnings", lambda *a, **k: calls.append(a)
    )

    session = _http.build_session(verify="/etc/ssl/ca.pem")

    assert session.verify == "/etc/ssl/ca.pem"
    # A truthy CA-bundle path is still "verifying", so no suppression.
    assert calls == []


def test_disable_insecure_warnings_is_idempotent(monkeypatch):
    calls = []
    monkeypatch.setattr(
        _http.urllib3, "disable_warnings", lambda *a, **k: calls.append(a)
    )

    _http.disable_insecure_warnings()
    _http.disable_insecure_warnings()
    _http.disable_insecure_warnings()

    assert len(calls) == 1


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("true", True),
        ("TRUE", True),
        ("yes", True),
        ("on", True),
        ("1", True),
        ("false", False),
        ("no", False),
        ("off", False),
        ("0", False),
        ("  true  ", True),
        ("/etc/ssl/ca-bundle.pem", "/etc/ssl/ca-bundle.pem"),
    ],
)
def test_parse_ssl_verify(raw, expected):
    assert _parse_ssl_verify(raw) == expected
