"""Mutation-killing pins for the boot-wiring seams in :mod:`mtui.mcp.main`.

A mutmut run showed 5x survivors in ``main()`` (and one in
``build_session()``) that ``tests/test_mcp_main.py`` never touches
because every collaborator there is a bare :class:`MagicMock` and the
existing assertions only check return codes / log text /
``assert_called_once()`` — so mutants that drop or swap an *argument*
at a call site (``cfg.merge_args(None)``, ``SessionRegistry(None,
...)``, ``build_session(cfg, None)``, ``FastMCP(name=<dropped>)``,
``build_tools(provider)`` missing ``mcp``, ``register_job_tools(mcp,
None)``, ``apply_profile(...)`` missing ``mcp``, ...) all sail
through unnoticed.

This file adds call-argument assertions for every boot-wiring seam:

* ``get_parser(sys)`` / ``parser.parse_args(sys.argv[1:])`` are called
  with the real ``sys`` module / real argv slice (not ``None``) --
  pinned via a wrapper spy since ``parse_args(None)`` is otherwise
  behaviorally equivalent (argparse falls back to ``sys.argv[1:]``
  internally);
* ``set_color_mode(args.color)``, ``Config(args.config)``,
  ``cfg.merge_args(args)`` receive the real parsed values;
* an invalid choice / a blocked ``mcp.server.fastmcp`` import still
  exit with the documented status codes;
* ``--debug`` raises both the app logger and the SDK's
  ``mcp.server.fastmcp`` logger to ``DEBUG``;
* the stdio vs. http provider selection: stdio calls
  ``build_session(cfg, logger)`` directly; http constructs a
  ``SessionRegistry(build_session, cfg, logger, max_sessions=...,
  idle_timeout=...)`` -- and whichever provider results is the one
  object threaded into ``build_tools`` / ``register_testreport_tools``
  / ``register_job_tools``;
* ``FastMCP(name="mtui", host=args.host, port=args.port)``;
* ``slim_registered_tools(mcp)`` and ``apply_profile(mcp, profile,
  allow=..., deny=...)`` at the end of boot;
* :func:`mtui.mcp.main.build_session` itself forwards the real logger
  (not ``None``) to :class:`McpSession`.

Every new test here was hand-verified to fail against a representative
hand-applied mutant (see the session's verification notes) before the
production file was restored byte-identical.

New file per project convention (do not add to ``test_mcp_main.py``).
Replicates that file's fake-``FastMCP`` and autouse logger-restore
conventions locally so this file stays hermetic and independent.
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock

import pytest

from mtui.mcp import main as mcp_main

# --------------------------------------------------------------------------- #
# Hermetic-state fixtures                                                     #
# --------------------------------------------------------------------------- #


@pytest.fixture(autouse=True)
def _restore_fastmcp_sdk_logger_level():
    """Undo ``--debug``'s mutation of the real ``mcp.server.fastmcp`` logger.

    Restore its level so this doesn't leak into other tests or repeated
    in-process pytest runs (e.g. under mutmut).
    """
    logger = logging.getLogger("mcp.server.fastmcp")
    saved_level = logger.level
    yield
    logger.setLevel(saved_level)


@pytest.fixture(autouse=True)
def _restore_mtui_mcp_logger():
    """Undo main()'s wiring of the process-global 'mtui-mcp' logger.

    main()'s first line is create_logger("mtui-mcp"), which unconditionally
    appends a handler and sets the level. The tests that drive the real
    main() (invalid-transport, missing-mcp-extra) reach it before returning,
    so without this restore each run leaks a handler and pins the level --
    unbounded growth under mutmut's repeated in-process runs.
    """
    logger = logging.getLogger("mtui-mcp")
    saved_handlers = logger.handlers[:]
    saved_level = logger.level
    yield
    logger.handlers[:] = saved_handlers
    logger.setLevel(saved_level)


# --------------------------------------------------------------------------- #
# Fake FastMCP (replicated from tests/test_mcp_main.py)                       #
# --------------------------------------------------------------------------- #


def _install_fake_fastmcp(
    monkeypatch: pytest.MonkeyPatch,
) -> tuple[MagicMock, MagicMock]:
    """Install a fake ``mcp.server.fastmcp.FastMCP`` for ``main()``.

    ``main()`` does ``from mcp.server.fastmcp import FastMCP`` lazily,
    so the stub has to live on ``sys.modules["mcp.server.fastmcp"]``
    before the call. ``run()`` is a no-op here; these tests never drive
    the server loop itself. Returns ``(fastmcp_cls, fastmcp_instance)``.
    """
    fastmcp_instance = MagicMock(name="FastMCP_instance")
    fastmcp_instance.run.side_effect = lambda *a, **kw: None
    fastmcp_cls = MagicMock(name="FastMCP_cls", return_value=fastmcp_instance)

    fake_module = MagicMock(name="mcp.server.fastmcp_module")
    fake_module.FastMCP = fastmcp_cls
    monkeypatch.setitem(__import__("sys").modules, "mcp.server.fastmcp", fake_module)
    return fastmcp_cls, fastmcp_instance


# --------------------------------------------------------------------------- #
# Full-boot stub environment                                                  #
# --------------------------------------------------------------------------- #


@pytest.fixture
def stub_environment(monkeypatch: pytest.MonkeyPatch) -> dict[str, Any]:
    """Patch every collaborator ``main()`` wires together, with recorders.

    Unlike ``tests/test_mcp_main.py``'s fixture of the same name (a
    different module -- no collision), this one also stubs
    ``SessionRegistry``, ``build_session``, ``register_job_tools``,
    ``slim_registered_tools``, ``apply_profile``, ``create_logger`` and
    ``set_color_mode`` so every boot-wiring seam can be asserted on by
    call args rather than merely "was called".
    """
    cfg = MagicMock(name="Config")
    cfg.mcp_session_cap = 7
    cfg.mcp_session_idle_timeout = 123.0
    cfg.mcp_tool_profile = "readonly"
    cfg.mcp_tools_allow = ("whoami",)
    cfg.mcp_tools_deny = ("edit",)
    config_cls = MagicMock(name="Config_cls", return_value=cfg)

    detect = MagicMock(name="detect_system", return_value=("sles", "15", "5.14"))

    stdio_provider = MagicMock(name="stdio_provider")
    build_session = MagicMock(name="build_session", return_value=stdio_provider)

    http_provider = MagicMock(name="http_provider")
    session_registry_cls = MagicMock(
        name="SessionRegistry_cls", return_value=http_provider
    )

    build_tools = MagicMock(name="build_tools", return_value=[])
    register_testreport_tools = MagicMock(name="register_testreport_tools")
    register_job_tools = MagicMock(name="register_job_tools")
    slim_registered_tools = MagicMock(name="slim_registered_tools", return_value=0)
    apply_profile = MagicMock(name="apply_profile", return_value=[])

    app_logger = MagicMock(name="app_logger")
    create_logger = MagicMock(name="create_logger", return_value=app_logger)

    set_color_mode = MagicMock(name="set_color_mode")

    monkeypatch.setattr(mcp_main, "Config", config_cls)
    monkeypatch.setattr(mcp_main, "detect_system", detect)
    monkeypatch.setattr(mcp_main, "build_session", build_session)
    monkeypatch.setattr(mcp_main, "SessionRegistry", session_registry_cls)
    monkeypatch.setattr(mcp_main, "build_tools", build_tools)
    monkeypatch.setattr(
        mcp_main, "register_testreport_tools", register_testreport_tools
    )
    monkeypatch.setattr(mcp_main, "register_job_tools", register_job_tools)
    monkeypatch.setattr(mcp_main, "slim_registered_tools", slim_registered_tools)
    monkeypatch.setattr(mcp_main, "apply_profile", apply_profile)
    monkeypatch.setattr(mcp_main, "create_logger", create_logger)
    monkeypatch.setattr(mcp_main, "set_color_mode", set_color_mode)

    return {
        "config": cfg,
        "config_cls": config_cls,
        "detect_system": detect,
        "build_session": build_session,
        "stdio_provider": stdio_provider,
        "SessionRegistry": session_registry_cls,
        "http_provider": http_provider,
        "build_tools": build_tools,
        "register_testreport_tools": register_testreport_tools,
        "register_job_tools": register_job_tools,
        "slim_registered_tools": slim_registered_tools,
        "apply_profile": apply_profile,
        "logger": app_logger,
        "create_logger": create_logger,
        "set_color_mode": set_color_mode,
    }


def _run(
    monkeypatch: pytest.MonkeyPatch, argv: list[str] | None = None
) -> tuple[int, MagicMock]:
    """Install the fake FastMCP and invoke ``main()`` with ``argv``."""
    _, fastmcp_instance = _install_fake_fastmcp(monkeypatch)
    monkeypatch.setattr("sys.argv", ["mtui-mcp", *(argv or [])])
    rc = mcp_main.main()
    return rc, fastmcp_instance


def _install_get_parser_spy(monkeypatch: pytest.MonkeyPatch) -> dict[str, Any]:
    """Wrap the real ``get_parser`` to record its arg and ``parse_args``'.

    ``parser.parse_args(None)`` is behaviorally equivalent to
    ``parser.parse_args(sys.argv[1:])`` (argparse falls back to
    ``sys.argv[1:]`` internally when ``args is None``), so the only way
    to pin "``main()`` passes the real slice, not ``None``" is a
    call-arg spy on the real objects, not a black-box outcome check.
    """
    real_get_parser = mcp_main.get_parser
    calls: dict[str, Any] = {}

    def spy(sys_module: Any) -> Any:
        calls["get_parser_arg"] = sys_module
        parser = real_get_parser(sys_module)
        real_parse_args = parser.parse_args

        def parse_args_spy(args: Any = None, namespace: Any = None) -> Any:
            calls["parse_args_arg"] = args
            ns = real_parse_args(args=args, namespace=namespace)
            calls["namespace"] = ns
            return ns

        parser.parse_args = parse_args_spy  # ty: ignore[invalid-assignment]
        return parser

    monkeypatch.setattr(mcp_main, "get_parser", spy)
    return calls


# --------------------------------------------------------------------------- #
# get_parser / parse_args / set_color_mode / Config / merge_args wiring       #
# --------------------------------------------------------------------------- #


def test_main_parses_real_argv_and_wires_namespace_into_config(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, Any],
) -> None:
    """``get_parser(sys)`` / ``parse_args(sys.argv[1:])`` get the real objects.

    And the resulting ``Namespace`` is the exact object handed to both
    ``Config(args.config)`` and ``cfg.merge_args(args)`` -- kills
    ``get_parser(None)``, ``parse_args(None)``, ``Config(None)`` and
    ``cfg.merge_args(None)`` mutants that a bare
    ``assert_called_once()`` would miss.
    """
    import sys as real_sys

    spy = _install_get_parser_spy(monkeypatch)

    rc, _ = _run(
        monkeypatch, argv=["--config", "/tmp/mtui-mcp-pins.cfg", "--color", "always"]
    )
    assert rc == 0

    assert spy["get_parser_arg"] is real_sys
    assert spy["parse_args_arg"] == real_sys.argv[1:]
    assert spy["parse_args_arg"] is not None

    namespace = spy["namespace"]
    assert namespace.config == Path("/tmp/mtui-mcp-pins.cfg")

    config_cls = stub_environment["config_cls"]
    config_cls.assert_called_once_with(namespace.config)

    cfg = stub_environment["config"]
    cfg.merge_args.assert_called_once_with(namespace)


def test_main_forwards_color_choice_to_set_color_mode(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, Any],
) -> None:
    """``set_color_mode(args.color)`` gets the real parsed choice."""
    rc, _ = _run(monkeypatch, argv=["--color", "never"])
    assert rc == 0
    stub_environment["set_color_mode"].assert_called_once_with("never")


def test_main_invalid_transport_choice_exits_status_2(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An invalid ``--transport`` choice makes argparse exit status 2.

    Exercised with NO stub environment: the parse failure happens
    before ``Config``/``FastMCP``/anything else is touched. This also
    pins ``get_parser(sys)`` end-to-end: if ``sys`` were swapped for
    ``None`` the parser's error-reporting path would crash with
    ``AttributeError`` instead of cleanly returning 2.
    """
    monkeypatch.setattr("sys.argv", ["mtui-mcp", "--transport", "bogus"])
    rc = mcp_main.main()
    assert rc == 2


def test_main_returns_2_when_mcp_extra_missing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A blocked SDK import returns exactly 2 (not some other code)."""
    monkeypatch.setitem(__import__("sys").modules, "mcp.server.fastmcp", None)
    monkeypatch.setattr("sys.argv", ["mtui-mcp"])
    rc = mcp_main.main()
    assert rc == 2


# --------------------------------------------------------------------------- #
# --debug wiring                                                              #
# --------------------------------------------------------------------------- #


def test_main_debug_flag_raises_app_logger_level(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, Any],
) -> None:
    """``--debug`` raises the app logger's level to DEBUG (real call arg)."""
    rc, _ = _run(monkeypatch, argv=["--debug"])
    assert rc == 0
    stub_environment["logger"].setLevel.assert_any_call(level=logging.DEBUG)


def test_main_debug_flag_raises_sdk_logger_to_debug(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, Any],
) -> None:
    """``--debug`` also raises the real ``mcp.server.fastmcp`` logger.

    Checked against the real, named, process-global logger object (not
    a mock) so a mutant swapping the logger name (or passing ``None``
    to ``getLogger``) is caught: it would leave *this* logger's own
    ``.level`` untouched (still whatever it was before the test).
    """
    rc, _ = _run(monkeypatch, argv=["--debug"])
    assert rc == 0
    assert logging.getLogger("mcp.server.fastmcp").level == logging.DEBUG


def test_main_without_debug_leaves_sdk_logger_alone(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, Any],
) -> None:
    """No ``--debug`` -> the SDK logger's level is left untouched."""
    logging.getLogger("mcp.server.fastmcp").setLevel(logging.WARNING)
    rc, _ = _run(monkeypatch, argv=[])
    assert rc == 0
    assert logging.getLogger("mcp.server.fastmcp").level == logging.WARNING


# --------------------------------------------------------------------------- #
# stdio vs. http provider selection                                          #
# --------------------------------------------------------------------------- #


def test_main_stdio_transport_builds_provider_via_build_session(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, Any],
) -> None:
    """stdio (default transport) calls ``build_session(cfg, logger)`` directly.

    And threads its return value through as the shared provider.
    """
    rc, _ = _run(monkeypatch, argv=[])
    assert rc == 0

    cfg = stub_environment["config"]
    logger = stub_environment["logger"]
    stub_environment["build_session"].assert_called_once_with(cfg, logger)
    stub_environment["SessionRegistry"].assert_not_called()

    provider = stub_environment["stdio_provider"]
    stub_environment["build_tools"].assert_called_once()
    assert stub_environment["build_tools"].call_args.args[1] is provider
    stub_environment["register_testreport_tools"].assert_called_once()
    assert stub_environment["register_testreport_tools"].call_args.args[1] is provider
    stub_environment["register_job_tools"].assert_called_once()
    assert stub_environment["register_job_tools"].call_args.args[1] is provider


def test_main_http_transport_builds_session_registry_with_cfg_bounds(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, Any],
) -> None:
    """``--transport http`` constructs a ``SessionRegistry`` from the real args.

    ``build_session`` factory, ``cfg``, ``logger`` and the cfg-derived
    session-cap / idle-timeout bounds -- and *that* becomes the shared
    provider threaded into every tool-registration call.
    """
    rc, _ = _run(monkeypatch, argv=["--transport", "http"])
    assert rc == 0

    cfg = stub_environment["config"]
    logger = stub_environment["logger"]
    stub_environment["SessionRegistry"].assert_called_once_with(
        stub_environment["build_session"],
        cfg,
        logger,
        max_sessions=7,
        idle_timeout=123.0,
    )
    stub_environment["build_session"].assert_not_called()

    provider = stub_environment["http_provider"]
    assert stub_environment["build_tools"].call_args.args[1] is provider
    assert stub_environment["register_testreport_tools"].call_args.args[1] is provider
    assert stub_environment["register_job_tools"].call_args.args[1] is provider


# --------------------------------------------------------------------------- #
# FastMCP construction                                                        #
# --------------------------------------------------------------------------- #


def test_main_constructs_fastmcp_with_name_and_default_host_port(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, Any],
) -> None:
    """``FastMCP(name="mtui", host=..., port=...)`` with argparse defaults."""
    fastmcp_cls, _ = _install_fake_fastmcp(monkeypatch)
    monkeypatch.setattr("sys.argv", ["mtui-mcp"])
    rc = mcp_main.main()
    assert rc == 0
    fastmcp_cls.assert_called_once_with(name="mtui", host="127.0.0.1", port=8000)


def test_main_constructs_fastmcp_with_custom_host_and_port(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, Any],
) -> None:
    """A custom ``--host``/``--port`` reaches the ``FastMCP`` constructor."""
    fastmcp_cls, _ = _install_fake_fastmcp(monkeypatch)
    monkeypatch.setattr("sys.argv", ["mtui-mcp", "--host", "0.0.0.0", "--port", "9001"])
    rc = mcp_main.main()
    assert rc == 0
    fastmcp_cls.assert_called_once_with(name="mtui", host="0.0.0.0", port=9001)

    fastmcp_instance = fastmcp_cls.return_value
    # The same instance is what gets threaded into every registration call.
    stub_environment["build_tools"].assert_called_once_with(
        fastmcp_instance, stub_environment["stdio_provider"]
    )
    stub_environment["register_testreport_tools"].assert_called_once_with(
        fastmcp_instance, stub_environment["stdio_provider"]
    )
    stub_environment["register_job_tools"].assert_called_once_with(
        fastmcp_instance, stub_environment["stdio_provider"]
    )


# --------------------------------------------------------------------------- #
# slim_registered_tools / apply_profile wiring                               #
# --------------------------------------------------------------------------- #


def test_main_slims_and_applies_profile_with_expected_args(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, Any],
) -> None:
    """``slim_registered_tools(mcp)`` then ``apply_profile(mcp, profile, ...)``.

    Called with ``allow=``/``deny=`` from the cfg-derived values, on the
    real ``FastMCP`` instance -- kills mutants dropping ``mcp`` or any of
    the three profile-related args.
    """
    fastmcp_cls, fastmcp_instance = _install_fake_fastmcp(monkeypatch)
    monkeypatch.setattr("sys.argv", ["mtui-mcp"])
    rc = mcp_main.main()
    assert rc == 0

    stub_environment["slim_registered_tools"].assert_called_once_with(fastmcp_instance)
    cfg = stub_environment["config"]
    stub_environment["apply_profile"].assert_called_once_with(
        fastmcp_instance,
        cfg.mcp_tool_profile,
        allow=cfg.mcp_tools_allow,
        deny=cfg.mcp_tools_deny,
    )


# --------------------------------------------------------------------------- #
# build_session() itself                                                      #
# --------------------------------------------------------------------------- #


def test_build_session_forwards_the_real_logger_not_none(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """``build_session`` forwards ``log`` unchanged (not ``None``) to ``McpSession``.

    And a *copy* of ``cfg`` (not the same object).
    """
    from types import SimpleNamespace

    session = MagicMock(name="McpSession_instance")
    session_cls = MagicMock(name="McpSession_cls", return_value=session)
    monkeypatch.setattr(mcp_main, "McpSession", session_cls)

    cfg = SimpleNamespace(distro="sles", ssl_verify=True)
    log = logging.getLogger("test.mtui.mcp.main.build_session")

    result = mcp_main.build_session(cfg, log)  # ty: ignore[invalid-argument-type]

    assert result is session
    session_cls.assert_called_once()
    passed_cfg, passed_log = session_cls.call_args.args
    assert passed_log is log
    assert passed_cfg is not cfg
    assert passed_cfg.distro == "sles"
