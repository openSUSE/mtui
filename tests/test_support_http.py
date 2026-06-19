"""Tests for the centralized HTTP timeout / TLS-verification helper."""

import ssl

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


# ---------------------------------------------------------------------------
# get_bytes
# ---------------------------------------------------------------------------


class _FakeResponse:
    def __init__(self, content: bytes, *, raise_exc: Exception | None = None) -> None:
        self.content = content
        self._raise_exc = raise_exc

    def raise_for_status(self) -> None:
        if self._raise_exc is not None:
            raise self._raise_exc


class _FakeSession:
    def __init__(self, response: _FakeResponse) -> None:
        self._response = response
        self.get_calls: list[tuple[str, dict]] = []

    def get(self, url: str, **kwargs):
        self.get_calls.append((url, kwargs))
        return self._response


def test_get_bytes_returns_response_content(monkeypatch):
    session = _FakeSession(_FakeResponse(b"payload-bytes"))
    monkeypatch.setattr(_http, "build_session", lambda verify: session)

    out = _http.get_bytes("https://h/file", verify=True)

    assert out == b"payload-bytes"


def test_get_bytes_passes_shared_timeout_by_default(monkeypatch):
    session = _FakeSession(_FakeResponse(b""))
    monkeypatch.setattr(_http, "build_session", lambda verify: session)

    _http.get_bytes("https://h/file", verify=False)

    assert session.get_calls[0][1]["timeout"] == _http.HTTP_TIMEOUT


def test_get_bytes_honors_verify_argument(monkeypatch):
    captured = {}

    def fake_build_session(verify):
        captured["verify"] = verify
        return _FakeSession(_FakeResponse(b""))

    monkeypatch.setattr(_http, "build_session", fake_build_session)

    _http.get_bytes("https://h/file", verify="/etc/ssl/ca.pem")

    assert captured["verify"] == "/etc/ssl/ca.pem"


def test_get_bytes_raises_on_http_error_status(monkeypatch):
    err = _http.requests.exceptions.HTTPError("404")
    session = _FakeSession(_FakeResponse(b"", raise_exc=err))
    monkeypatch.setattr(_http, "build_session", lambda verify: session)

    with pytest.raises(_http.requests.exceptions.HTTPError):
        _http.get_bytes("https://h/file", verify=True)


# ---------------------------------------------------------------------------
# is_ssl_verification_error / ssl_verification_hint
# ---------------------------------------------------------------------------


def test_is_ssl_verification_error_detects_wrapped_cert_error():
    inner = ssl.SSLCertVerificationError(
        1, "[SSL: CERTIFICATE_VERIFY_FAILED] certificate verify failed"
    )
    outer = _http.requests.exceptions.SSLError("wrapped")
    outer.__cause__ = inner
    assert _http.is_ssl_verification_error(outer) is True


def test_is_ssl_verification_error_detects_requests_sslerror():
    assert (
        _http.is_ssl_verification_error(_http.requests.exceptions.SSLError("boom"))
        is True
    )


def test_is_ssl_verification_error_matches_message_fallback():
    # A generic error whose text mentions the cert failure still matches.
    assert (
        _http.is_ssl_verification_error(
            RuntimeError("... CERTIFICATE_VERIFY_FAILED ...")
        )
        is True
    )


def test_is_ssl_verification_error_false_for_other_errors():
    assert (
        _http.is_ssl_verification_error(
            _http.requests.exceptions.ConnectTimeout("timed out")
        )
        is False
    )
    assert _http.is_ssl_verification_error(ValueError("nope")) is False


def test_is_ssl_verification_error_handles_cause_cycle():
    # A self-referential cause must not loop forever.
    a = RuntimeError("a")
    b = RuntimeError("b")
    a.__cause__ = b
    b.__cause__ = a
    assert _http.is_ssl_verification_error(a) is False


def test_ssl_verification_hint_mentions_remedies():
    msg = _http.ssl_verification_hint("src.suse.de")
    assert "src.suse.de" in msg
    assert "ssl_verify = false" in msg
    assert "CA" in msg


def test_ssl_verification_hint_without_host():
    msg = _http.ssl_verification_hint()
    assert "ssl_verify = false" in msg
