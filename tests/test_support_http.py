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


def test_default_pool_size_matches_threadpool_default(monkeypatch):
    # Mirrors ThreadPoolExecutor's own default of min(32, cpu + 4).
    monkeypatch.setattr(_http.os, "process_cpu_count", lambda: 4)
    assert _http.default_pool_size() == 8

    monkeypatch.setattr(_http.os, "process_cpu_count", lambda: 100)
    assert _http.default_pool_size() == 32  # capped at 32

    # os.process_cpu_count() can return None; fall back to 1 + 4.
    monkeypatch.setattr(_http.os, "process_cpu_count", lambda: None)
    assert _http.default_pool_size() == 5


def test_build_session_sizes_connection_pool(monkeypatch):
    monkeypatch.setattr(_http, "default_pool_size", lambda: 17)

    session = _http.build_session(verify=True)

    for scheme in ("https://", "http://"):
        adapter = session.get_adapter(f"{scheme}example.test")
        assert isinstance(adapter, _http.requests.adapters.HTTPAdapter)
        # pool_maxsize is forwarded to the underlying urllib3 pool manager
        # connection_pool_kw; assert on that public surface rather than the
        # adapter's private _pool_maxsize attribute.
        assert adapter.poolmanager.connection_pool_kw["maxsize"] == 17


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


@pytest.mark.parametrize("raw", ["true", "TRUE", "yes", "on", "1", "  true  "])
def test_parse_ssl_verify_true_spellings_equal_the_default(raw, monkeypatch):
    """An explicit true is deliberately identical to an unset option.

    Writing out the documented default must never change behaviour: with a
    system bundle present both prefer it, without one both are certifi
    ``True``.
    """
    import mtui.support.config as _config

    monkeypatch.setattr(_config, "system_ca_bundle", lambda: None)
    assert _parse_ssl_verify(raw) is True
    monkeypatch.setattr(_config, "system_ca_bundle", lambda: "/sys/ca.pem")
    assert _parse_ssl_verify(raw) == "/sys/ca.pem"


@pytest.mark.parametrize("raw", ["false", "no", "off", "0"])
def test_parse_ssl_verify_false_spellings(raw):
    assert _parse_ssl_verify(raw) is False


def test_parse_ssl_verify_blank_disables_with_warning(caplog):
    """A blank value keeps its historical requests semantics (verify off)."""
    with caplog.at_level("WARNING", logger="mtui.config"):
        assert _parse_ssl_verify("  ") is False
    assert any("blank ssl_verify" in r.message for r in caplog.records)


def test_parse_ssl_verify_existing_file(tmp_path):
    ca = tmp_path / "ca.pem"
    ca.write_text("dummy")
    assert _parse_ssl_verify(str(ca)) == str(ca)


def test_parse_ssl_verify_relative_path_is_absolutised(tmp_path, monkeypatch):
    """A relative CA path must survive a later chdir_to_template_dir."""
    (tmp_path / "ca.pem").write_text("dummy")
    monkeypatch.chdir(tmp_path)
    assert _parse_ssl_verify("ca.pem") == str(tmp_path / "ca.pem")


def test_parse_ssl_verify_hashed_directory_accepted(tmp_path):
    # OpenSSL only consults hash-named entries in a capath directory.
    (tmp_path / "0a1b2c3d.0").write_text("dummy")
    assert _parse_ssl_verify(str(tmp_path)) == str(tmp_path)


def test_parse_ssl_verify_unhashed_directory_rejected(tmp_path):
    """A directory without c_rehash-ed entries could never verify anything."""
    (tmp_path / "some-ca.pem").write_text("dummy")
    with pytest.raises(ValueError, match="c_rehash"):
        _parse_ssl_verify(str(tmp_path))


def test_parse_ssl_verify_expands_tilde(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    (tmp_path / "ca.pem").write_text("dummy")
    assert _parse_ssl_verify("~/ca.pem") == str(tmp_path / "ca.pem")


@pytest.mark.parametrize("bad", ["false1", "ture", "/nonexistent/ca.pem"])
def test_parse_ssl_verify_invalid_value_raises(bad):
    """Neither a boolean spelling nor an existing path: reject at parse time.

    ``false1`` is the reproduced field bug — previously it flowed verbatim
    into ``requests`` and died at the first HTTPS call with an opaque
    ``OSError`` instead of a config error.
    """
    with pytest.raises(ValueError, match="true/yes/on/1"):
        _parse_ssl_verify(bad)


# ---------------------------------------------------------------------------
# system_ca_bundle
# ---------------------------------------------------------------------------


def _no_default_verify_paths(monkeypatch, tmp_path):
    """Point the interpreter's OpenSSL cafile at a nonexistent path."""
    from types import SimpleNamespace

    monkeypatch.setattr(
        _http.ssl,
        "get_default_verify_paths",
        lambda: SimpleNamespace(cafile=str(tmp_path / "openssl-absent.pem")),
    )


def test_system_ca_bundle_prefers_interpreter_cafile(tmp_path, monkeypatch):
    """ssl.get_default_verify_paths().cafile wins over the fallback list.

    It reflects the interpreter's real OpenSSL configuration and honours
    the SSL_CERT_FILE override.
    """
    from types import SimpleNamespace

    openssl = tmp_path / "openssl.pem"
    openssl.write_text("dummy")
    fallback = tmp_path / "fallback.pem"
    fallback.write_text("dummy")
    monkeypatch.setattr(
        _http.ssl, "get_default_verify_paths", lambda: SimpleNamespace(cafile=str(openssl))
    )
    monkeypatch.setattr(_http, "_SYSTEM_CA_BUNDLES", (str(fallback),))
    assert _http.system_ca_bundle() == str(openssl)


def test_system_ca_bundle_falls_back_to_wellknown_paths(tmp_path, monkeypatch):
    _no_default_verify_paths(monkeypatch, tmp_path)
    missing = tmp_path / "missing.pem"
    present = tmp_path / "present.pem"
    present.write_text("dummy")
    monkeypatch.setattr(_http, "_SYSTEM_CA_BUNDLES", (str(missing), str(present)))
    assert _http.system_ca_bundle() == str(present)


def test_system_ca_bundle_none_when_no_candidate_exists(tmp_path, monkeypatch):
    _no_default_verify_paths(monkeypatch, tmp_path)
    monkeypatch.setattr(_http, "_SYSTEM_CA_BUNDLES", (str(tmp_path / "no.pem"),))
    assert _http.system_ca_bundle() is None


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


def test_ssl_verification_hint_names_the_system_bundle(monkeypatch):
    """With a distribution bundle present, the hint gives its exact path."""
    monkeypatch.setattr(_http, "system_ca_bundle", lambda: "/etc/ssl/ca-bundle.pem")
    msg = _http.ssl_verification_hint()
    assert "ssl_verify = /etc/ssl/ca-bundle.pem" in msg


def test_ssl_verification_hint_generic_without_system_bundle(monkeypatch):
    monkeypatch.setattr(_http, "system_ca_bundle", lambda: None)
    msg = _http.ssl_verification_hint()
    assert "/path/to/ca.pem" in msg
