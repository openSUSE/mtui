"""Tests for :mod:`mtui.mcp.main`.

Focused on the Ctrl-C / graceful shutdown contract:

* a bare ``KeyboardInterrupt`` raised by :meth:`FastMCP.run` exits ``0``
  with a single ``mtui-mcp: shutting down`` log line and no traceback;
* a :class:`BaseExceptionGroup` whose leaves are all shutdown sentinels
  (``KeyboardInterrupt`` / ``SystemExit`` / ``asyncio.CancelledError``)
  is also treated as a clean shutdown — anyio wraps Ctrl-C delivered to
  an active task group this way, so a bare ``except KeyboardInterrupt``
  would miss it;
* a :class:`BaseExceptionGroup` containing a real error still hits the
  crash path (``return 1``, ``mtui-mcp crashed`` logged);
* a ``KeyboardInterrupt`` during preload (``session.load_update``) is
  caught before the server is reached and exits ``0`` cleanly.

The tests stub out :class:`Config`, :func:`detect_system`, the
:class:`McpSession` constructor, and ``build_tools`` /
``register_testreport_tools`` so the module-level flow can be exercised
without touching real config files, OBS, SSH, or fastmcp internals.
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any
from unittest.mock import MagicMock

import pytest

from mtui.mcp import main as mcp_main

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
    cfg.kernel = False
    cfg.auto = False
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


def _run_with_fake_fastmcp(
    monkeypatch: pytest.MonkeyPatch,
    fastmcp_run: Any,
    argv: list[str] | None = None,
) -> tuple[int, MagicMock]:
    """Invoke ``main()`` with a fake ``FastMCP`` whose ``run`` does ``fastmcp_run``.

    ``fastmcp_run`` may be a callable (invoked with no args) or an
    exception instance/class to raise. Returns ``(exit_code, fastmcp_instance)``.
    """
    fastmcp_instance = MagicMock(name="FastMCP_instance")
    if isinstance(fastmcp_run, BaseException) or (
        isinstance(fastmcp_run, type) and issubclass(fastmcp_run, BaseException)
    ):
        fastmcp_instance.run.side_effect = fastmcp_run
    else:
        fastmcp_instance.run.side_effect = lambda *a, **kw: fastmcp_run()

    fastmcp_cls = MagicMock(name="FastMCP_cls", return_value=fastmcp_instance)

    # mtui.mcp.main does ``from fastmcp import FastMCP`` lazily inside
    # main(); patch the module so the import resolves to our stub.
    fake_module = MagicMock(name="fastmcp_module")
    fake_module.FastMCP = fastmcp_cls
    monkeypatch.setitem(__import__("sys").modules, "fastmcp", fake_module)

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
# Pre-server Ctrl-C (preload)                                                 #
# --------------------------------------------------------------------------- #


def test_main_returns_zero_on_keyboardinterrupt_during_preload(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, MagicMock],
    caplog: pytest.LogCaptureFixture,
) -> None:
    """Ctrl-C while ``load_update`` is running exits 0 without starting the server."""
    caplog.set_level(logging.INFO, logger="mtui-mcp")

    stub_environment["session"].load_update.side_effect = KeyboardInterrupt

    # Build a Namespace-like object so we can bypass argparse (the real
    # parser would need a valid OBS review id for ``-a``).
    fake_args = MagicMock(
        config=None,
        color="never",
        debug=False,
        sut=None,
        transport="stdio",
        host="127.0.0.1",
        port=8000,
    )
    # ``update`` has to walk the ``kind`` branch; give it a stub.
    fake_update = MagicMock()
    fake_update.kind = "auto"
    fake_args.update = fake_update

    parser = MagicMock(name="parser")
    parser.parse_args.return_value = fake_args
    monkeypatch.setattr(mcp_main, "get_parser", lambda _sys: parser)

    fastmcp_instance = MagicMock(name="FastMCP_instance")
    fastmcp_cls = MagicMock(name="FastMCP_cls", return_value=fastmcp_instance)
    fake_module = MagicMock(name="fastmcp_module")
    fake_module.FastMCP = fastmcp_cls
    monkeypatch.setitem(__import__("sys").modules, "fastmcp", fake_module)

    rc = mcp_main.main()

    assert rc == 0
    assert any("shutting down" in r.message for r in caplog.records)
    # Server must never have been constructed or run.
    assert not fastmcp_cls.called
    assert not fastmcp_instance.run.called


def test_main_returns_zero_on_keyboardinterrupt_during_autoconnect(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, MagicMock],
    caplog: pytest.LogCaptureFixture,
) -> None:
    """Ctrl-C while iterating the autoconnect SUT list exits 0 cleanly."""
    caplog.set_level(logging.INFO, logger="mtui-mcp")

    # Force the add_host command lookup to succeed with a stub, then
    # make session._run_sync raise KI on the first invocation.
    fake_add_host_cls = MagicMock(name="add_host_cls")
    monkeypatch.setattr(
        mcp_main.Command,
        "registry",
        {"add_host": fake_add_host_cls},
    )
    stub_environment["session"]._run_sync.side_effect = KeyboardInterrupt

    fake_sut = MagicMock()
    fake_sut.print_args.return_value = "host1.example.com"

    fake_args = MagicMock(
        config=None,
        color="never",
        debug=False,
        sut=[fake_sut],
        update=None,
        transport="stdio",
        host="127.0.0.1",
        port=8000,
    )

    parser = MagicMock(name="parser")
    parser.parse_args.return_value = fake_args
    monkeypatch.setattr(mcp_main, "get_parser", lambda _sys: parser)

    fastmcp_instance = MagicMock(name="FastMCP_instance")
    fastmcp_cls = MagicMock(name="FastMCP_cls", return_value=fastmcp_instance)
    fake_module = MagicMock(name="fastmcp_module")
    fake_module.FastMCP = fastmcp_cls
    monkeypatch.setitem(__import__("sys").modules, "fastmcp", fake_module)

    rc = mcp_main.main()

    assert rc == 0
    assert any("shutting down" in r.message for r in caplog.records)
    assert not fastmcp_cls.called
    assert not fastmcp_instance.run.called


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
    """Verify HTTP transport reaches ``mcp.run(transport=...)`` with host/port."""
    rc, fastmcp_instance = _run_with_fake_fastmcp(
        monkeypatch,
        lambda: None,
        argv=["--transport", "http", "--host", "0.0.0.0", "--port", "9000"],
    )
    assert rc == 0
    fastmcp_instance.run.assert_called_once_with(
        transport="http", host="0.0.0.0", port=9000
    )
