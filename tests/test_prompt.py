"""Tests for the prompt_toolkit-backed interactive REPL.

These tests target :mod:`mtui.cli.repl` directly.

The loop is driven by feeding text into a
:class:`~prompt_toolkit.input.PipeInput` plumbed through ``PromptSession``
via the ``_input``/``_output`` test seam on :class:`CommandPrompt`.
"""

import subprocess
from typing import Any
from unittest.mock import MagicMock

import pytest
from prompt_toolkit.input import create_pipe_input
from prompt_toolkit.output import DummyOutput

from mtui.cli import repl
from mtui.cli.argparse import ArgsParseFailureError
from mtui.support import messages
from mtui.test_reports.null_report import NullTestReport
from mtui.types import Workflow


def _bind_do(p: repl.CommandPrompt, name: str, handler: Any) -> None:
    """Bind ``do_<name>`` and register the command on ``p``.

    Centralises the ``setattr`` + ``p.commands[name]`` plumbing the
    cmdloop tests need so the ``ty`` ``unresolved-attribute`` and
    ``invalid-assignment`` warnings live in exactly one place.
    """
    setattr(p, f"do_{name}", handler)
    p.commands[name] = MagicMock()


def _load_mock_report(
    p: repl.CommandPrompt,
    *,
    rrid: str = "SUSE:Maintenance:1:1",
    workflow: Workflow = Workflow.MANUAL,
    targets: Any = None,
) -> MagicMock:
    """Load a mock (non-null) TestReport into the prompt's registry.

    ``metadata`` / ``targets`` are read-only properties resolving through the
    :class:`TemplateRegistry`, so tests inject a report by adding it to the
    registry instead of assigning ``p.metadata`` directly.
    """
    report = MagicMock()
    report.id = rrid
    report.workflow = workflow
    report.targets = MagicMock() if targets is None else targets
    p.templates.add(report)
    p.templates.set_active(rrid)
    return report


def _make_prompt(
    *,
    workflow: Workflow = Workflow.MANUAL,
    pipe_input=None,
) -> repl.CommandPrompt:
    """Build a ``CommandPrompt`` with stock magic-mocked collaborators.

    When ``pipe_input`` is supplied, it is plumbed into ``PromptSession``
    via the ``_input`` kwarg so tests can feed lines through the real
    session machinery without touching the controlling TTY.
    """
    config = MagicMock()
    log = MagicMock()
    sys = MagicMock()
    display_factory = MagicMock()
    p = repl.CommandPrompt(
        config,
        log,
        sys,
        display_factory,
        _input=pipe_input,
        _output=DummyOutput() if pipe_input is not None else None,
    )
    # Workflow mode now lives on the active TestReport, not on config.
    p.metadata.workflow = workflow
    return p


# --------------------------------------------------------------------------- #
# Construction & basic attributes                                             #
# --------------------------------------------------------------------------- #


def test_command_prompt_init():
    """``CommandPrompt`` exposes the documented attribute surface."""
    config = MagicMock()
    log = MagicMock()
    sys = MagicMock()
    display_factory = MagicMock()

    p = repl.CommandPrompt(config, log, sys, display_factory)

    assert p.config is config
    assert p.log is log
    assert p.sys is sys
    assert p.display is display_factory.return_value
    assert p.interactive is True
    assert p.prompt == "mtui-empty>"
    assert isinstance(p.metadata, NullTestReport)


# --------------------------------------------------------------------------- #
# Command registration                                                        #
# --------------------------------------------------------------------------- #


# --------------------------------------------------------------------------- #
# Subcommand registration                                                     #
# --------------------------------------------------------------------------- #


def test_dispatching():
    """``_add_subcommand`` binds working ``do_/help_/complete_`` closures."""
    p = _make_prompt()

    mock_command = MagicMock()
    mock_command.command = "test_command"
    mock_argparser = MagicMock()
    mock_command.argparser.return_value = mock_argparser

    p._add_subcommand(mock_command)  # type: ignore[arg-type]

    # do_
    p.do_test_command("test_args")
    mock_command.parse_args.assert_called_with("test_args", p.sys)
    mock_command.return_value.run.assert_called_once()

    # help_
    p.help_test_command()
    mock_argparser.print_help.assert_called_once()

    # complete_
    p.complete_test_command("text", "line", 0, 1)
    mock_command.complete.assert_called_once()


def test_add_subcommand_duplicate_raises():
    """Re-registering a command name is a hard error."""
    p = _make_prompt()
    cmd_a = MagicMock()
    cmd_a.command = "dup"
    cmd_b = MagicMock()
    cmd_b.command = "dup"
    p._add_subcommand(cmd_a)  # type: ignore[arg-type]
    with pytest.raises(repl.CommandAlreadyBoundError, match="dup"):
        p._add_subcommand(cmd_b)  # type: ignore[arg-type]


def test_add_subcommand_binds_methods_to_instance():
    """Closures are stored in ``self.__dict__`` at registration time."""
    p = _make_prompt()
    cmd = MagicMock()
    cmd.command = "alpha"
    p._add_subcommand(cmd)  # type: ignore[arg-type]
    assert "do_alpha" in p.__dict__
    assert "help_alpha" in p.__dict__
    assert "complete_alpha" in p.__dict__


def test_dash_in_command_name_dispatches():
    """Command names containing ``-`` (e.g. ``dash-cmd``) round-trip."""
    p = _make_prompt()
    cmd = MagicMock()
    cmd.command = "dash-cmd"
    p._add_subcommand(cmd)  # type: ignore[arg-type]
    do = getattr(p, "do_dash-cmd")
    do("args")
    cmd.parse_args.assert_called_with("args", p.sys)
    cmd.return_value.run.assert_called_once()


def test_do_handles_argsparse_failure():
    """``do_*`` swallows ``ArgsParseFailureError`` and does not invoke the command."""
    p = _make_prompt()
    cmd = MagicMock()
    cmd.command = "boom"
    cmd.parse_args.side_effect = ArgsParseFailureError()
    p._add_subcommand(cmd)  # type: ignore[arg-type]
    p.do_boom("--bad")
    cmd.assert_not_called()


def test_complete_logs_and_reraises(caplog):
    """``complete_*`` logs the exception then re-raises."""
    p = _make_prompt()
    cmd = MagicMock()
    cmd.command = "alpha"
    cmd.complete.side_effect = RuntimeError("comp-fail")
    p._add_subcommand(cmd)  # type: ignore[arg-type]
    with (
        caplog.at_level("ERROR", logger="mtui.prompt"),
        pytest.raises(RuntimeError, match="comp-fail"),
    ):
        p.complete_alpha("text", "line", 0, 1)
    assert any(r.exc_info is not None for r in caplog.records)


def test_get_names_includes_registered_commands():
    """``get_names`` surfaces ``do_X`` and ``help_X`` for every registered command."""
    p = _make_prompt()
    cmd = MagicMock()
    cmd.command = "alpha"
    p._add_subcommand(cmd)  # type: ignore[arg-type]
    names = p.get_names()
    assert "do_alpha" in names
    assert "help_alpha" in names


# --------------------------------------------------------------------------- #
# cmdloop control flow                                                        #
# --------------------------------------------------------------------------- #


def _seed_quit(p: repl.CommandPrompt) -> MagicMock:
    """Bind a ``do_quit`` that raises :class:`QuitLoopError` and return the mock.

    Used by every cmdloop test that needs to terminate the loop cleanly:
    queue a final ``"quit"`` line, and let the dispatch error path catch
    the ``QuitLoopError`` exactly like the production ``Quit`` command
    does on its own ``self.sys.exit(0)`` path.
    """
    quit_mock = MagicMock(side_effect=repl.QuitLoopError)
    _bind_do(p, "quit", quit_mock)
    return quit_mock


def _mock_prompt(monkeypatch, p: repl.CommandPrompt, lines: list[Any]) -> MagicMock:
    """Make ``p._session.prompt`` return successive ``lines``.

    Each invocation returns the next item; tests that need an exception
    on the input phase (``KeyboardInterrupt``, ``EOFError``) can replace
    a list element with the exception type itself — :class:`MagicMock`
    will raise it.
    """
    mock = MagicMock(side_effect=lines)
    monkeypatch.setattr(p._session, "prompt", mock)
    return mock


def test_cmdloop_keyboard_interrupt_reprompts(monkeypatch):
    """``KeyboardInterrupt`` at the prompt clears the line and reprompts.

    Ctrl-C on a partial input line must not tear down the REPL: the
    input phase raises ``_LoopContinueError`` so the loop skips dispatch
    and reads the next line. We feed Ctrl-C then ``"quit"`` and assert
    the prompt was asked twice (i.e. the loop survived the interrupt).
    """
    p = _make_prompt()
    _seed_quit(p)
    # First prompt call raises Ctrl-C; second returns "quit" to exit.
    mock = _mock_prompt(monkeypatch, p, [KeyboardInterrupt, "quit"])

    p.cmdloop()

    # Two prompt reads: the interrupted one and the one that returns quit.
    assert mock.call_count == 2
    assert p.interactive is True


def test_cmdloop_keyboard_interrupt_during_command_returns_to_prompt(
    monkeypatch, caplog
):
    """Ctrl-C raised by a dispatched command aborts that command, not the REPL.

    Regression: ``add_host`` (and any other command that blocks on a
    paramiko connect) used to propagate ``KeyboardInterrupt`` through
    the dispatch ``except Exception`` clause -- which doesn't catch
    ``BaseException`` -- and tore the whole mtui process down with a
    traceback.
    """
    p = _make_prompt()
    _bind_do(p, "slow", MagicMock(side_effect=KeyboardInterrupt))
    _seed_quit(p)
    _mock_prompt(monkeypatch, p, ["slow", "quit"])
    with caplog.at_level("WARNING", logger="mtui.prompt"):
        p.cmdloop()  # must return cleanly via the seeded "quit"
    assert any("interrupted by user" in r.message for r in caplog.records)


def test_cmdloop_quit_loop_exits(monkeypatch):
    """``QuitLoopError`` from a dispatched command exits the loop."""
    p = _make_prompt()
    _seed_quit(p)
    _mock_prompt(monkeypatch, p, ["quit"])
    p.cmdloop()  # must return cleanly


def test_cmdloop_eof_dispatches_eof_command(monkeypatch):
    """``EOFError`` (Ctrl-D) dispatches the registered ``EOF`` command."""
    p = _make_prompt()
    do_eof = MagicMock(side_effect=repl.QuitLoopError)
    _bind_do(p, "EOF", do_eof)
    _mock_prompt(monkeypatch, p, [EOFError])
    p.cmdloop()
    do_eof.assert_called_once_with("")


def test_cmdloop_user_message_logs_error_then_quits(monkeypatch, caplog):
    """``UserMessage`` is logged at error level (non-debug path) and the loop continues."""
    p = _make_prompt()
    _bind_do(p, "boom", MagicMock(side_effect=messages.NoRefhostsDefinedError()))
    _seed_quit(p)
    _mock_prompt(monkeypatch, p, ["boom", "quit"])
    with caplog.at_level("ERROR", logger="mtui.prompt"):
        p.cmdloop()
    assert any("No refhosts defined" in r.message for r in caplog.records)


def test_cmdloop_user_message_logs_traceback_in_debug(monkeypatch, caplog):
    """When debug is enabled, ``UserMessage`` is logged with a traceback."""
    p = _make_prompt()
    _bind_do(p, "boom", MagicMock(side_effect=messages.NoRefhostsDefinedError()))
    _seed_quit(p)
    _mock_prompt(monkeypatch, p, ["boom", "quit"])
    with caplog.at_level("DEBUG", logger="mtui.prompt"):
        p.cmdloop()
    assert any(r.exc_info is not None for r in caplog.records)


def test_cmdloop_called_process_error_logs_and_continues(monkeypatch, caplog):
    """``subprocess.CalledProcessError`` follows the same path as ``UserMessage``."""
    p = _make_prompt()
    err = subprocess.CalledProcessError(1, ["false"])
    _bind_do(p, "boom", MagicMock(side_effect=err))
    _seed_quit(p)
    _mock_prompt(monkeypatch, p, ["boom", "quit"])
    with caplog.at_level("ERROR", logger="mtui.prompt"):
        p.cmdloop()
    assert any("false" in r.message or "1" in r.message for r in caplog.records)


def test_cmdloop_unexpected_error_logs_and_continues(monkeypatch, caplog):
    """Generic ``Exception`` is logged as 'Unexpected error' and the loop continues."""
    p = _make_prompt()
    _bind_do(p, "boom", MagicMock(side_effect=RuntimeError("kaboom")))
    _seed_quit(p)
    _mock_prompt(monkeypatch, p, ["boom", "quit"])
    with caplog.at_level("ERROR", logger="mtui.prompt"):
        p.cmdloop()
    assert any(
        "Unexpected error" in r.message and "kaboom" in r.message
        for r in caplog.records
    )


def test_cmdloop_unexpected_error_logs_traceback_in_debug(monkeypatch, caplog):
    """In debug mode the unexpected-error path uses ``logger.exception``."""
    p = _make_prompt()
    _bind_do(p, "boom", MagicMock(side_effect=RuntimeError("kaboom")))
    _seed_quit(p)
    _mock_prompt(monkeypatch, p, ["boom", "quit"])
    with caplog.at_level("DEBUG", logger="mtui.prompt"):
        p.cmdloop()
    assert any(
        r.exc_info is not None and "Unexpected error" in r.message
        for r in caplog.records
    )


def test_cmdloop_unknown_command_logs_warning(monkeypatch, caplog):
    """An unrecognised command logs ``unknown command`` and keeps looping."""
    p = _make_prompt()
    _seed_quit(p)
    _mock_prompt(monkeypatch, p, ["definitely-not-a-command", "quit"])
    with caplog.at_level("WARNING", logger="mtui.prompt"):
        p.cmdloop()
    assert any("unknown command" in r.message for r in caplog.records)


def test_cmdloop_empty_line_is_ignored(monkeypatch):
    """An empty input line must not crash the loop."""
    p = _make_prompt()
    _seed_quit(p)
    _mock_prompt(monkeypatch, p, ["", "   ", "quit"])
    p.cmdloop()


def test_cmdloop_intro_is_printed(monkeypatch):
    """The ``intro`` banner, when provided, is emitted through ``println``."""
    p = _make_prompt()
    println = MagicMock()
    monkeypatch.setattr(p, "println", println)
    _seed_quit(p)
    _mock_prompt(monkeypatch, p, ["quit"])
    p.cmdloop(intro="hello world")
    # The first call is the intro; later calls are post-quit (none).
    println.assert_any_call("hello world")


def test_cmdloop_drives_real_session_via_pipe_input(monkeypatch):
    """Smoke test the full PromptSession path with a PipeInput.

    Exercises the seam end-to-end: a real ``PromptSession`` reads from
    the pipe, returns the line, the loop dispatches it, then the next
    iteration hits ``EOF`` (Ctrl-D) and the registered ``do_EOF`` raises
    ``QuitLoopError``.
    """
    with create_pipe_input() as pipe_input:
        p = _make_prompt(pipe_input=pipe_input)

        do_hello = MagicMock()
        _bind_do(p, "hello", do_hello)

        def quit_via_eof(_arg):
            raise repl.QuitLoopError

        _bind_do(p, "EOF", quit_via_eof)

        pipe_input.send_text("hello world\n")
        pipe_input.send_text("\x04")  # Ctrl-D → EOFError → "EOF" dispatch

        p.cmdloop()

    do_hello.assert_called_once_with("world")


# --------------------------------------------------------------------------- #
# Misc helpers                                                                #
# --------------------------------------------------------------------------- #


def test_notify_user_calls_notification_display(monkeypatch):
    """``notify_user`` wraps ``notification.display``; a plain class has no icon."""
    p = _make_prompt()
    display = MagicMock()
    monkeypatch.setattr(repl.notification, "display", display)
    p.notify_user("hello", class_="info")
    display.assert_called_once_with("MTUI", "hello", None)


def test_notify_user_maps_error_class_to_icon(monkeypatch):
    """The historical error class maps to a freedesktop icon name."""
    p = _make_prompt()
    display = MagicMock()
    monkeypatch.setattr(repl.notification, "display", display)
    p.notify_user("boom", class_="stock_dialog-error")
    display.assert_called_once_with("MTUI", "boom", "dialog-error")


def test_postcmd_short_circuits_for_null_test_report():
    """Until a real test report is loaded, ``postcmd`` must not touch the prompt."""
    p = _make_prompt()
    assert isinstance(p.metadata, NullTestReport)
    original_prompt = p.prompt
    assert p.postcmd(False, "noop") is False
    assert p.prompt == original_prompt


def test_postcmd_updates_prompt_when_metadata_loaded():
    """With a real test report, ``postcmd`` refreshes the prompt."""
    p = _make_prompt()
    _load_mock_report(p, workflow=Workflow.MANUAL)  # not a NullTestReport
    p.postcmd(False, "noop")
    assert p.prompt == "mtui> "


def test_emptyline_returns_false():
    """``emptyline`` must not stop the loop and must not repeat the last command."""
    p = _make_prompt()
    assert p.emptyline() is False


def test_set_prompt_normal_mode():
    """No auto, no kernel: prompt prefix is ``mtui``."""
    p = _make_prompt()
    p.set_prompt()
    assert p.prompt == "mtui> "


def test_set_prompt_auto_mode():
    """``Workflow.AUTO`` renders the ``mtui-auto`` prefix."""
    p = _make_prompt(workflow=Workflow.AUTO)
    p.set_prompt()
    assert p.prompt == "mtui-auto> "


def test_set_prompt_kernel_mode():
    """``Workflow.KERNEL`` renders the ``mtui-kernel`` prefix."""
    p = _make_prompt(workflow=Workflow.KERNEL)
    p.set_prompt()
    assert p.prompt == "mtui-kernel> "


# --------------------------------------------------------------------------- #
# Bottom toolbar                                                              #
# --------------------------------------------------------------------------- #


def test_bottom_toolbar_manual_mode_zero_hosts():
    """Default config + no loaded report → ``manual`` / 0 hosts / no active."""
    p = _make_prompt()
    # NullTestReport.targets is an empty HostsGroup which supports len().
    assert len(p.targets) == 0
    assert p._bottom_toolbar() == (" mode: manual  hosts: 0  templates: 0  active: - ")


def test_bottom_toolbar_shows_template_count_and_active():
    """Loaded templates surface as ``templates: N`` / ``active: <RRID>``."""
    p = _make_prompt()
    _load_mock_report(p, rrid="SUSE:Maintenance:1:1", targets={})
    _load_mock_report(p, rrid="SUSE:Maintenance:2:2", targets={})

    out = p._bottom_toolbar()

    assert " templates: 2 " in out
    # _load_mock_report sets each loaded report active, so the last one wins.
    assert " active: SUSE:Maintenance:2:2 " in out


def test_bottom_toolbar_kernel_mode():
    """``Workflow.KERNEL`` renders the ``kernel`` mode label."""
    p = _make_prompt(workflow=Workflow.KERNEL)
    assert " mode: kernel " in p._bottom_toolbar()


def test_bottom_toolbar_auto_mode():
    """``Workflow.AUTO`` renders the ``auto`` mode label."""
    p = _make_prompt(workflow=Workflow.AUTO)
    assert " mode: auto " in p._bottom_toolbar()


def test_bottom_toolbar_renders_host_count():
    """``len(self.targets)`` is reflected in the ``hosts:`` field."""
    p = _make_prompt()
    targets = MagicMock()
    targets.__len__ = MagicMock(return_value=3)
    _load_mock_report(p, targets=targets)
    assert " hosts: 3 " in p._bottom_toolbar()


def test_bottom_toolbar_handles_targets_without_len():
    """Defensive guard: a ``targets`` value lacking ``__len__`` yields ``?``."""
    p = _make_prompt()

    class _NoLen:
        pass

    _load_mock_report(p, targets=_NoLen())
    assert " hosts: ? " in p._bottom_toolbar()


def test_bottom_toolbar_active_uses_metadata_id():
    """A loaded test report surfaces its RRID via the ``active:`` field."""
    p = _make_prompt()
    _load_mock_report(p, rrid="SUSE:Maintenance:12345:67890")
    assert " active: SUSE:Maintenance:12345:67890 " in p._bottom_toolbar()


def test_bottom_toolbar_empty_metadata_id_active_dash():
    """A ``NullTestReport``-style empty id collapses ``active:`` to ``-``."""
    p = _make_prompt()
    # NullTestReport.id returns ""; the default p.metadata is already that.
    assert isinstance(p.metadata, NullTestReport)
    assert p.metadata.id == ""
    assert " active: - " in p._bottom_toolbar()


def test_load_update_swaps_metadata_and_targets():
    """``load_update`` installs the new test report and resets the prompt."""
    p = _make_prompt()
    new_targets = MagicMock()
    new_tr = MagicMock(targets=new_targets)
    update = MagicMock()
    update.make_testreport.return_value = new_tr
    p.load_update(update, autoconnect=False)
    update.make_testreport.assert_called_once_with(
        p.config, False, p.interactive, prompter=p.prompter
    )
    assert p.metadata is new_tr
    assert p.targets is new_targets
    assert p.prompt.endswith("> ")
    assert ":" not in p.prompt
