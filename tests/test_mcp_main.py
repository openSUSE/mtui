"""Tests for :mod:`mtui.mcp.main`.

Focused on the Ctrl-C / graceful shutdown contract:

* a bare ``KeyboardInterrupt`` raised by
  :meth:`mcp.server.fastmcp.FastMCP.run` exits ``0`` with a single
  ``mtui-mcp: shutting down`` log line and no traceback;
* a :class:`BaseExceptionGroup` whose leaves are all shutdown sentinels
  (``KeyboardInterrupt`` / ``SystemExit`` / ``asyncio.CancelledError``)
  is also treated as a clean shutdown — anyio wraps Ctrl-C delivered to
  an active task group this way, so a bare ``except KeyboardInterrupt``
  would miss it;
* a :class:`BaseExceptionGroup` containing a real error still hits the
  crash path (``return 1``, ``mtui-mcp crashed`` logged).

The tests stub out :class:`Config`, :func:`detect_system`, the
:class:`McpSession` constructor, and ``build_tools`` /
``register_testreport_tools`` so the module-level flow can be exercised
without touching real config files, OBS, SSH, or the
:mod:`mcp.server.fastmcp` internals.
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any
from unittest.mock import MagicMock

import pytest

from mtui.mcp import main as mcp_main


@pytest.fixture(autouse=True)
def _restore_mtui_mcp_logger():
    """Undo main()'s wiring of the process-global 'mtui-mcp' logger.

    ``main()`` attaches a real stream handler (bound to the current,
    soon-to-be-closed capture stream) and sets the level; left in place
    it leaks into later tests and into repeated in-process pytest runs
    under mutmut.
    """
    logger = logging.getLogger("mtui-mcp")
    saved_handlers = logger.handlers[:]
    saved_level = logger.level
    yield
    logger.handlers[:] = saved_handlers
    logger.setLevel(saved_level)


# --------------------------------------------------------------------------- #
# Helpers                                                                     #
# --------------------------------------------------------------------------- #


@pytest.fixture
def stub_environment(monkeypatch: pytest.MonkeyPatch) -> dict[str, MagicMock]:
    """Patch the heavyweight collaborators main() pulls in.

    Returns a dict of the mocks so individual tests can assert on calls
    or swap in side effects (e.g. make ``load_update`` raise).
    """
    cfg = MagicMock(name="Config")
    config_cls = MagicMock(name="Config_cls", return_value=cfg)
    detect = MagicMock(name="detect_system", return_value=("sles", "15", "5.14"))
    session = MagicMock(name="McpSession")
    session_cls = MagicMock(name="McpSession_cls", return_value=session)
    build = MagicMock(name="build_tools")
    register = MagicMock(name="register_testreport_tools")

    monkeypatch.setattr(mcp_main, "Config", config_cls)
    monkeypatch.setattr(mcp_main, "detect_system", detect)
    monkeypatch.setattr(mcp_main, "McpSession", session_cls)
    monkeypatch.setattr(mcp_main, "build_tools", build)
    monkeypatch.setattr(mcp_main, "register_testreport_tools", register)

    return {
        "config": cfg,
        "session": session,
        "build_tools": build,
        "register_testreport_tools": register,
    }


def _install_fake_fastmcp(
    monkeypatch: pytest.MonkeyPatch,
    fastmcp_run: Any = None,
) -> tuple[MagicMock, MagicMock]:
    """Install a fake ``mcp.server.fastmcp.FastMCP`` class for ``main()``.

    ``main()`` does ``from mcp.server.fastmcp import FastMCP`` lazily,
    so the stub has to live on ``sys.modules["mcp.server.fastmcp"]``
    before the call. Returns ``(fastmcp_cls, fastmcp_instance)``.

    ``fastmcp_run`` may be a callable (invoked with no args) or an
    exception instance/class to raise from ``mcp.run(...)``. When
    ``None``, ``mcp.run(...)`` is a no-op (used by the preload tests
    that never actually reach the server loop).
    """
    fastmcp_instance = MagicMock(name="FastMCP_instance")
    if fastmcp_run is None:
        fastmcp_instance.run.side_effect = lambda *a, **kw: None
    elif isinstance(fastmcp_run, BaseException) or (
        isinstance(fastmcp_run, type) and issubclass(fastmcp_run, BaseException)
    ):
        fastmcp_instance.run.side_effect = fastmcp_run
    else:
        fastmcp_instance.run.side_effect = lambda *a, **kw: fastmcp_run()

    fastmcp_cls = MagicMock(name="FastMCP_cls", return_value=fastmcp_instance)

    # The SDK's package layout is ``mcp.server.fastmcp``. Injecting a
    # stub module at that dotted path makes the lazy ``from ... import
    # FastMCP`` resolve to ``fastmcp_cls`` without ever touching the
    # real SDK code.
    fake_module = MagicMock(name="mcp.server.fastmcp_module")
    fake_module.FastMCP = fastmcp_cls
    monkeypatch.setitem(__import__("sys").modules, "mcp.server.fastmcp", fake_module)
    return fastmcp_cls, fastmcp_instance


def _run_with_fake_fastmcp(
    monkeypatch: pytest.MonkeyPatch,
    fastmcp_run: Any,
    argv: list[str] | None = None,
) -> tuple[int, MagicMock]:
    """Invoke ``main()`` with a fake ``FastMCP`` whose ``run`` does ``fastmcp_run``.

    ``fastmcp_run`` may be a callable (invoked with no args) or an
    exception instance/class to raise. Returns ``(exit_code, fastmcp_instance)``.
    """
    _, fastmcp_instance = _install_fake_fastmcp(monkeypatch, fastmcp_run)
    monkeypatch.setattr("sys.argv", ["mtui-mcp", *(argv or [])])
    rc = mcp_main.main()
    return rc, fastmcp_instance


# --------------------------------------------------------------------------- #
# Shutdown helper                                                             #
# --------------------------------------------------------------------------- #


def test_is_clean_shutdown_group_bare_keyboardinterrupt() -> None:
    assert mcp_main._is_clean_shutdown_group(KeyboardInterrupt())  # noqa: SLF001


def test_is_clean_shutdown_group_bare_cancelled() -> None:
    assert mcp_main._is_clean_shutdown_group(asyncio.CancelledError())  # noqa: SLF001


def test_is_clean_shutdown_group_bare_runtimeerror_is_not_clean() -> None:
    assert not mcp_main._is_clean_shutdown_group(RuntimeError("boom"))  # noqa: SLF001


def test_is_clean_shutdown_group_group_of_keyboardinterrupts() -> None:
    eg = BaseExceptionGroup("ki", [KeyboardInterrupt(), KeyboardInterrupt()])
    assert mcp_main._is_clean_shutdown_group(eg)  # noqa: SLF001


def test_is_clean_shutdown_group_nested_group_all_clean() -> None:
    inner = BaseExceptionGroup("inner", [asyncio.CancelledError()])
    outer = BaseExceptionGroup("outer", [KeyboardInterrupt(), inner])
    assert mcp_main._is_clean_shutdown_group(outer)  # noqa: SLF001


def test_is_clean_shutdown_group_mixed_group_is_not_clean() -> None:
    eg = BaseExceptionGroup("mixed", [KeyboardInterrupt(), RuntimeError("boom")])
    assert not mcp_main._is_clean_shutdown_group(eg)  # noqa: SLF001


# --------------------------------------------------------------------------- #
# Server-loop Ctrl-C                                                          #
# --------------------------------------------------------------------------- #


def test_main_returns_zero_on_keyboardinterrupt_from_run(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, MagicMock],
    caplog: pytest.LogCaptureFixture,
) -> None:
    """Bare ``KeyboardInterrupt`` from ``mcp.run()`` -> clean exit."""
    caplog.set_level(logging.INFO, logger="mtui-mcp")
    rc, fastmcp_instance = _run_with_fake_fastmcp(monkeypatch, KeyboardInterrupt)
    assert rc == 0
    assert fastmcp_instance.run.called
    assert any("shutting down" in r.message for r in caplog.records)
    # Crucially: no "crashed" line.
    assert not any("crashed" in r.message for r in caplog.records)


def test_main_returns_zero_on_baseexceptiongroup_of_keyboardinterrupt(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, MagicMock],
    caplog: pytest.LogCaptureFixture,
) -> None:
    """``anyio.run`` wraps Ctrl-C in a BaseExceptionGroup -> still clean."""
    caplog.set_level(logging.INFO, logger="mtui-mcp")
    eg = BaseExceptionGroup("anyio shutdown", [KeyboardInterrupt()])
    rc, _ = _run_with_fake_fastmcp(monkeypatch, eg)
    assert rc == 0
    assert any("shutting down" in r.message for r in caplog.records)
    assert not any("crashed" in r.message for r in caplog.records)


def test_main_returns_zero_on_mixed_shutdown_group(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, MagicMock],
    caplog: pytest.LogCaptureFixture,
) -> None:
    """A group of KI + CancelledError is still treated as a clean exit."""
    caplog.set_level(logging.INFO, logger="mtui-mcp")
    eg = BaseExceptionGroup(
        "anyio mixed", [KeyboardInterrupt(), asyncio.CancelledError()]
    )
    rc, _ = _run_with_fake_fastmcp(monkeypatch, eg)
    assert rc == 0
    assert any("shutting down" in r.message for r in caplog.records)


def test_main_returns_one_on_baseexceptiongroup_with_real_error(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, MagicMock],
    caplog: pytest.LogCaptureFixture,
) -> None:
    """A group containing a real error must NOT be silently swallowed."""
    caplog.set_level(logging.ERROR, logger="mtui-mcp")
    eg = BaseExceptionGroup("anyio crash", [RuntimeError("boom")])
    rc, _ = _run_with_fake_fastmcp(monkeypatch, eg)
    assert rc == 1
    assert any("crashed" in r.message for r in caplog.records)


def test_main_returns_one_on_plain_exception(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, MagicMock],
    caplog: pytest.LogCaptureFixture,
) -> None:
    """A plain non-shutdown exception keeps the existing crash contract."""
    caplog.set_level(logging.ERROR, logger="mtui-mcp")
    rc, _ = _run_with_fake_fastmcp(monkeypatch, RuntimeError("kaboom"))
    assert rc == 1
    assert any("crashed" in r.message for r in caplog.records)


# --------------------------------------------------------------------------- #
# Default-flow sanity (no KI) -- guards against the new try/except hiding bugs
# --------------------------------------------------------------------------- #


def test_main_happy_path_returns_zero_after_clean_run(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, MagicMock],
) -> None:
    """If ``mcp.run()`` returns normally the function returns 0."""
    rc, fastmcp_instance = _run_with_fake_fastmcp(monkeypatch, lambda: None)
    assert rc == 0
    assert fastmcp_instance.run.called
    stub_environment["build_tools"].assert_called_once()
    stub_environment["register_testreport_tools"].assert_called_once()


def test_main_happy_path_http_transport(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, MagicMock],
) -> None:
    """``--transport http`` maps to ``mcp.run(transport="streamable-http")``.

    The user-facing flag stays ``http`` for back-compat with the
    standalone fastmcp era; under the SDK, host/port are constructor
    settings on :class:`FastMCP` (verified separately below) and
    ``run`` only takes ``transport``/``mount_path``.
    """
    rc, fastmcp_instance = _run_with_fake_fastmcp(
        monkeypatch,
        lambda: None,
        argv=["--transport", "http", "--host", "0.0.0.0", "--port", "9000"],
    )
    assert rc == 0
    fastmcp_instance.run.assert_called_once_with(transport="streamable-http")


# --------------------------------------------------------------------------- #
# Boot-time InsecureRequestWarning suppression                                #
# --------------------------------------------------------------------------- #


def test_main_suppresses_insecure_warning_when_verify_off(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, MagicMock],
) -> None:
    """``ssl_verify = false`` -> the urllib3 warning is silenced at boot.

    The MCP SDK wraps every request handler in
    ``warnings.catch_warnings(record=True)``, which snapshots and
    restores ``warnings.filters`` per request. Installing the ignore
    filter lazily on the first request loses it on that request's exit
    (and the helper's idempotency guard then blocks re-installation),
    so it must be installed before the server loop starts.
    """
    disable = MagicMock(name="disable_insecure_warnings")
    monkeypatch.setattr(mcp_main, "disable_insecure_warnings", disable)
    stub_environment["config"].ssl_verify = False

    rc, _ = _run_with_fake_fastmcp(monkeypatch, lambda: None)

    assert rc == 0
    disable.assert_called_once_with()


@pytest.mark.parametrize("ssl_verify", [True, None, "/etc/ssl/ca-bundle.pem"])
def test_main_keeps_insecure_warning_when_verifying(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, MagicMock],
    ssl_verify: object,
) -> None:
    """When verification is on (or a CA bundle), the warning stays active.

    A truthy ``ssl_verify`` (``True`` or a CA-bundle path) and the
    unset default (``None`` -> resolves to ``True``) all keep the
    insecure-request warning, so a genuine misconfiguration is never
    masked.
    """
    disable = MagicMock(name="disable_insecure_warnings")
    monkeypatch.setattr(mcp_main, "disable_insecure_warnings", disable)
    stub_environment["config"].ssl_verify = ssl_verify

    rc, _ = _run_with_fake_fastmcp(monkeypatch, lambda: None)

    assert rc == 0
    disable.assert_not_called()


def test_boot_suppression_survives_sdk_catch_warnings_cycle(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, MagicMock],
) -> None:
    """End-to-end: the real suppression survives per-request snapshots.

    This drives the *actual* ``disable_insecure_warnings`` (not a mock)
    through ``main()`` with verification off, then simulates the SDK's
    ``warnings.catch_warnings(record=True)`` request wrapper twice. The
    bug was that a filter installed inside the first request is
    discarded on its exit, so the second request re-records the warning;
    installing it at boot means both requests record nothing.
    """
    import warnings

    from urllib3.exceptions import InsecureRequestWarning

    from mtui.support import http as _http

    # Reset the helper's module-level idempotency guard so this test's
    # boot install is the one that takes effect, not a leftover from an
    # earlier test in the session.
    monkeypatch.setattr(_http, "_warnings_disabled", False)
    stub_environment["config"].ssl_verify = False

    rc, _ = _run_with_fake_fastmcp(monkeypatch, lambda: None)
    assert rc == 0

    def simulated_request() -> int:
        # Mirror mcp/server/lowlevel/server.py: each handler runs inside
        # ``catch_warnings(record=True)`` and re-emits what it recorded.
        with warnings.catch_warnings(record=True) as recorded:
            warnings.warn(
                "Unverified HTTPS request", InsecureRequestWarning, stacklevel=2
            )
            return len(recorded)

    assert simulated_request() == 0
    assert simulated_request() == 0
