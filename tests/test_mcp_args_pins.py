"""Mutation-killing pins for :func:`mtui.mcp.args.get_parser`.

There was no dedicated test file for ``mtui/mcp/args.py`` at all: a full
mutmut run left ~95 surviving mutants in code the suite executes only
incidentally (``test_mcp_main.py`` exercises ``--transport``/``--host``/
``--port`` on one happy-path test, but never asserts the parsed
Namespace's actual values or types). The tests here pin, for every flag
``get_parser`` defines:

* the parser's default ``Namespace`` in one shot, so a flipped
  ``default=`` (``"auto"`` -> ``None``, ``"127.0.0.1"`` -> ``None``,
  ``8000`` -> ``8001``, ``False`` -> ``True``/``None``, ``"stdio"`` ->
  ``None``) cannot survive without failing an assertion;
* ``type=Path`` really coerces ``-t``/``--template_dir`` and
  ``-c``/``--config`` to :class:`pathlib.Path` (not a raw ``str``);
* ``type=int`` really coerces ``-w``/``--connection_timeout`` and
  ``--port`` to ``int`` (not a raw ``str``), and rejects non-numeric
  input;
* every short/long flag pair is wired to the SAME option (dropping or
  renaming either form breaks parsing of that form specifically);
* ``--color``/``--transport`` ``choices=[...]`` accepts every listed
  value and rejects an unlisted one with ``ArgsParseFailureError``;
* ``-V``/``--version`` (``action=_VerboseVersionAction``) prints all
  four version lines and raises ``ArgsParseFailureError(status=0)``
  instead of silently storing a string;
* ``-d``/``--debug`` is a real ``store_true`` (default ``False``,
  ``True`` when passed).

A companion test drives :func:`mtui.mcp.main.main` end-to-end with a
fake ``FastMCP`` (replicating the fixture in ``test_mcp_main.py``, per
the "new tests go in a new file" rule) to assert ``FastMCP`` is
constructed with the parsed ``host``/``port`` values, not just that
``mcp.run(...)`` was called.

Each test was verified to fail against a hand-applied representative
mutant (confirmed via ``mutmut show`` against the real survivor set)
before the pristine code was restored.
"""

from __future__ import annotations

import io
import logging
from collections.abc import Mapping
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock

import pytest

pytest.importorskip("mcp")

from mtui.cli.argparse import ArgsParseFailureError  # noqa: E402
from mtui.mcp import main as mcp_main  # noqa: E402
from mtui.mcp.args import get_parser  # noqa: E402


class _FakeSys:
    """Minimal stand-in for the ``sys`` module ``get_parser`` takes.

    ``ArgumentParser`` only ever touches ``.stdout``/``.stderr`` on the
    object it is handed (see :class:`mtui.cli.argparse.ArgumentParser`),
    so that is all this needs to provide to capture ``-V`` output and
    argparse error messages without touching the real ``sys.stdout``.
    """

    def __init__(self) -> None:
        self.stdout = io.StringIO()
        self.stderr = io.StringIO()


# --------------------------------------------------------------------------- #
# Defaults: one assertion block that pins every default= in one shot          #
# --------------------------------------------------------------------------- #


def test_get_parser_defaults_with_no_args() -> None:
    """No argv at all -> every flag lands on its documented default."""
    parser = get_parser(_FakeSys())
    ns = parser.parse_args([])

    assert ns.template_dir is None
    assert ns.connection_timeout is None
    assert ns.debug is False
    assert ns.config is None
    assert ns.color == "auto"
    assert ns.gitea_token is None
    assert ns.transport == "stdio"
    assert ns.host == "127.0.0.1"
    assert ns.port == 8000
    assert isinstance(ns.port, int)


# --------------------------------------------------------------------------- #
# type=Path coercion (-t/--template_dir, -c/--config)                        #
# --------------------------------------------------------------------------- #


def test_template_dir_is_coerced_to_path_short_and_long_forms() -> None:
    parser = get_parser(_FakeSys())

    ns_short = parser.parse_args(["-t", "/tmp/short_template_dir"])
    assert ns_short.template_dir == Path("/tmp/short_template_dir")
    assert isinstance(ns_short.template_dir, Path)

    ns_long = parser.parse_args(["--template_dir", "/tmp/long_template_dir"])
    assert ns_long.template_dir == Path("/tmp/long_template_dir")
    assert isinstance(ns_long.template_dir, Path)


def test_config_is_coerced_to_path_short_and_long_forms() -> None:
    parser = get_parser(_FakeSys())

    ns_short = parser.parse_args(["-c", "/tmp/short_config"])
    assert ns_short.config == Path("/tmp/short_config")
    assert isinstance(ns_short.config, Path)

    ns_long = parser.parse_args(["--config", "/tmp/long_config"])
    assert ns_long.config == Path("/tmp/long_config")
    assert isinstance(ns_long.config, Path)


# --------------------------------------------------------------------------- #
# type=int coercion (-w/--connection_timeout, --port)                        #
# --------------------------------------------------------------------------- #


def test_connection_timeout_is_coerced_to_int_short_and_long_forms() -> None:
    parser = get_parser(_FakeSys())

    ns_short = parser.parse_args(["-w", "45"])
    assert ns_short.connection_timeout == 45
    assert isinstance(ns_short.connection_timeout, int)

    ns_long = parser.parse_args(["--connection_timeout", "90"])
    assert ns_long.connection_timeout == 90
    assert isinstance(ns_long.connection_timeout, int)


def test_port_is_coerced_to_int_and_rejects_non_numeric() -> None:
    parser = get_parser(_FakeSys())

    ns = parser.parse_args(["--port", "9001"])
    assert ns.port == 9001
    assert isinstance(ns.port, int)

    with pytest.raises(ArgsParseFailureError):
        parser.parse_args(["--port", "not-a-number"])


# --------------------------------------------------------------------------- #
# -d/--debug store_true                                                      #
# --------------------------------------------------------------------------- #


def test_debug_flag_short_and_long_forms() -> None:
    parser = get_parser(_FakeSys())

    assert parser.parse_args([]).debug is False
    assert parser.parse_args(["-d"]).debug is True
    assert parser.parse_args(["--debug"]).debug is True


# --------------------------------------------------------------------------- #
# -g/--gitea_token, --host: plain str options, both forms wired              #
# --------------------------------------------------------------------------- #


def test_gitea_token_short_and_long_forms() -> None:
    parser = get_parser(_FakeSys())

    assert parser.parse_args(["-g", "tok-short"]).gitea_token == "tok-short"
    assert parser.parse_args(["--gitea_token", "tok-long"]).gitea_token == "tok-long"


def test_host_default_and_override() -> None:
    parser = get_parser(_FakeSys())

    assert parser.parse_args([]).host == "127.0.0.1"
    ns = parser.parse_args(["--host", "0.0.0.0"])
    assert ns.host == "0.0.0.0"
    assert isinstance(ns.host, str)


# --------------------------------------------------------------------------- #
# choices=[...] validation (--color, --transport)                            #
# --------------------------------------------------------------------------- #


def test_color_accepts_every_documented_choice() -> None:
    parser = get_parser(_FakeSys())
    for choice in ("auto", "always", "never"):
        assert parser.parse_args(["--color", choice]).color == choice


def test_color_rejects_value_outside_choices() -> None:
    parser = get_parser(_FakeSys())
    with pytest.raises(ArgsParseFailureError):
        parser.parse_args(["--color", "banana"])


def test_transport_accepts_every_documented_choice() -> None:
    parser = get_parser(_FakeSys())
    assert parser.parse_args(["--transport", "stdio"]).transport == "stdio"
    assert parser.parse_args(["--transport", "http"]).transport == "http"


def test_transport_rejects_value_outside_choices() -> None:
    parser = get_parser(_FakeSys())
    with pytest.raises(ArgsParseFailureError):
        parser.parse_args(["--transport", "sse"])


# --------------------------------------------------------------------------- #
# -V/--version: _VerboseVersionAction prints and exits cleanly               #
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("flag", ["-V", "--version"])
def test_version_flag_prints_all_four_lines_and_exits_zero(flag: str) -> None:
    fake_sys = _FakeSys()
    parser = get_parser(fake_sys)

    with pytest.raises(ArgsParseFailureError) as exc_info:
        parser.parse_args([flag])

    assert exc_info.value.status == 0
    out = fake_sys.stdout.getvalue()
    lines = out.splitlines()
    assert len(lines) == 4
    assert lines[0].startswith("mtui ")
    assert lines[1].startswith("Python ")
    assert lines[2].startswith("paramiko ")
    assert lines[3].startswith("openqa-client ")


def test_version_flag_does_not_reach_the_server_loop() -> None:
    """``-V`` must exit cleanly (status 0) from ``parse_args`` itself.

    Asserting the status, not merely that *some* ArgsParseFailureError is
    raised, is what gives this independent kill power: if
    ``action=_VerboseVersionAction`` were dropped, ``-V`` becomes a plain
    ``store`` optional and ``parse_args(["-V"])`` errors with
    "expected one argument", which the custom parser reports as
    ``ArgsParseFailureError(status=2)`` -- so a bare ``pytest.raises``
    would pass on the mutant. Pinning ``status == 0`` fails it.
    """
    parser = get_parser(_FakeSys())
    with pytest.raises(ArgsParseFailureError) as exc_info:
        parser.parse_args(["-V"])
    assert exc_info.value.status == 0


# --------------------------------------------------------------------------- #
# End-to-end: FastMCP is constructed with the parsed host/port (as int)      #
# --------------------------------------------------------------------------- #
#
# Replicates the fake-FastMCP fixture from tests/test_mcp_main.py (kept in
# this new file per the "no edits to existing test files" rule) but adds
# the assertion that file's happy-path http test never made: that
# ``FastMCP(...)`` itself -- not just ``mcp.run(...)`` -- receives the
# parsed ``host``/``port``, with ``port`` as a real ``int``.


@pytest.fixture(autouse=True)
def _restore_mtui_mcp_logger():
    """Undo main()'s wiring of the process-global 'mtui-mcp' logger.

    Mirrors the identical fixture in ``test_mcp_main.py``: ``main()``
    attaches a real stream handler and sets the logger level; left in
    place it leaks into later tests and into repeated in-process pytest
    runs under mutmut.
    """
    logger = logging.getLogger("mtui-mcp")
    saved_handlers = logger.handlers[:]
    saved_level = logger.level
    yield
    logger.handlers[:] = saved_handlers
    logger.setLevel(saved_level)


@pytest.fixture
def stub_environment(monkeypatch: pytest.MonkeyPatch) -> dict[str, MagicMock]:
    """Patch the heavyweight collaborators main() pulls in."""
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
) -> tuple[MagicMock, MagicMock]:
    """Install a fake ``mcp.server.fastmcp.FastMCP`` whose ``run`` no-ops."""
    fastmcp_instance = MagicMock(name="FastMCP_instance")
    fastmcp_instance.run.side_effect = lambda *a, **kw: None

    fastmcp_cls = MagicMock(name="FastMCP_cls", return_value=fastmcp_instance)

    fake_module = MagicMock(name="mcp.server.fastmcp_module")
    fake_module.FastMCP = fastmcp_cls
    monkeypatch.setitem(__import__("sys").modules, "mcp.server.fastmcp", fake_module)
    return fastmcp_cls, fastmcp_instance


def test_main_constructs_fastmcp_with_parsed_host_and_port_as_int(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, MagicMock],
) -> None:
    fastmcp_cls, fastmcp_instance = _install_fake_fastmcp(monkeypatch)
    monkeypatch.setattr(
        "sys.argv",
        ["mtui-mcp", "--transport", "http", "--host", "0.0.0.0", "--port", "9000"],
    )

    rc = mcp_main.main()

    assert rc == 0
    fastmcp_cls.assert_called_once_with(name="mtui", host="0.0.0.0", port=9000)
    call_kwargs: Mapping[str, Any] = fastmcp_cls.call_args.kwargs
    assert isinstance(call_kwargs["port"], int)
    fastmcp_instance.run.assert_called_once_with(transport="streamable-http")


def test_main_constructs_fastmcp_with_default_host_and_port_under_stdio(
    monkeypatch: pytest.MonkeyPatch,
    stub_environment: dict[str, MagicMock],
) -> None:
    """No ``--transport``/``--host``/``--port`` -> the argparse defaults flow through."""
    fastmcp_cls, fastmcp_instance = _install_fake_fastmcp(monkeypatch)
    monkeypatch.setattr("sys.argv", ["mtui-mcp"])

    rc = mcp_main.main()

    assert rc == 0
    fastmcp_cls.assert_called_once_with(name="mtui", host="127.0.0.1", port=8000)
    assert isinstance(fastmcp_cls.call_args.kwargs["port"], int)
    fastmcp_instance.run.assert_called_once_with()
