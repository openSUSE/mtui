import subprocess
from unittest.mock import MagicMock

import pytest

from mtui import messages, prompt
from mtui.argparse import ArgsParseFailureError
from mtui.template.nulltestreport import NullTestReport


def _make_prompt(*, auto: bool = False, kernel: bool = False) -> prompt.CommandPrompt:
    """Build a ``CommandPrompt`` with stock magic-mocked collaborators."""
    config = MagicMock()
    config.auto = auto
    config.kernel = kernel
    log = MagicMock()
    sys = MagicMock()
    display_factory = MagicMock()
    return prompt.CommandPrompt(config, log, sys, display_factory)


def test_command_prompt_init():
    """Test CommandPrompt initialization."""
    config = MagicMock()
    log = MagicMock()
    sys = MagicMock()
    display_factory = MagicMock()

    p = prompt.CommandPrompt(config, log, sys, display_factory)

    assert p.config == config
    assert p.log == log
    assert p.sys == sys
    assert p.display == display_factory.return_value


def test_set_prompt():
    """Test set_prompt."""
    config = MagicMock()
    config.auto = False
    config.kernel = False
    log = MagicMock()
    sys = MagicMock()
    display_factory = MagicMock()

    p = prompt.CommandPrompt(config, log, sys, display_factory)
    p.set_prompt("test_session")

    assert p.prompt == "mtui:test_session> "


def test_cmd_queue():
    """Test CmdQueue."""
    term = MagicMock()
    queue = prompt.CmdQueue(["test_cmd"], "mtui> ", term)

    cmd = queue.pop(0)

    assert cmd == "test_cmd"
    term.stdout.write.assert_called_with("mtui> test_cmd\n")


def test_dispatching():
    """Test command, help, and completion dispatching."""
    config = MagicMock()
    log = MagicMock()
    sys = MagicMock()
    display_factory = MagicMock()

    p = prompt.CommandPrompt(config, log, sys, display_factory)

    mock_command = MagicMock()
    mock_command.command = "test_command"
    mock_argparser = MagicMock()
    mock_command.argparser.return_value = mock_argparser

    p._add_subcommand(mock_command)  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]

    # Test do_
    do_method = p.do_test_command
    do_method("test_args")
    mock_command.parse_args.assert_called_with("test_args", sys)
    mock_command.return_value.assert_called_once()

    # Test help_
    help_method = p.help_test_command
    help_method()
    mock_argparser.print_help.assert_called_once()

    # Test complete_
    complete_method = p.complete_test_command
    complete_method("text", "line", 0, 1)
    mock_command.complete.assert_called_once()


def test_cmdloop_keyboard_interrupt(monkeypatch):
    """Test that cmdloop handles KeyboardInterrupt."""
    config = MagicMock()
    log = MagicMock()
    sys = MagicMock()
    display_factory = MagicMock()

    p = prompt.CommandPrompt(config, log, sys, display_factory)

    # Raise KeyboardInterrupt on the first call, then QuitLoopError
    monkeypatch.setattr(
        "cmd.Cmd.cmdloop",
        MagicMock(side_effect=[KeyboardInterrupt, prompt.QuitLoopError]),
    )

    p.cmdloop()

    assert p.interactive is True
    assert p.cmdqueue == []


def test_cmdloop_quit_loop(monkeypatch):
    """Test that cmdloop handles QuitLoopError."""
    config = MagicMock()
    log = MagicMock()
    sys = MagicMock()
    display_factory = MagicMock()

    p = prompt.CommandPrompt(config, log, sys, display_factory)

    monkeypatch.setattr("cmd.Cmd.cmdloop", MagicMock(side_effect=prompt.QuitLoopError))

    p.cmdloop()  # Should exit cleanly


def test_notify_user_calls_notification_display(monkeypatch):
    """``notify_user`` is a thin wrapper around ``notification.display``."""
    p = _make_prompt()
    display = MagicMock()
    monkeypatch.setattr(prompt.notification, "display", display)
    p.notify_user("hello", class_="info")
    display.assert_called_once_with("MTUI", "hello", "info")


def test_read_history_swallows_oserror(monkeypatch, caplog):
    """A missing/unreadable history file is logged at debug and ignored."""
    monkeypatch.setattr(
        prompt.readline,
        "read_history_file",
        MagicMock(side_effect=OSError("no such file")),
    )
    with caplog.at_level("DEBUG", logger="mtui.prompt"):
        # Construction calls ``_read_history``; must not raise.
        _make_prompt()
    assert any("history file" in r.message for r in caplog.records)


def test_add_subcommand_duplicate_raises():
    """Re-registering a command name is a hard error."""
    p = _make_prompt()
    cmd_a = MagicMock()
    cmd_a.command = "dup"
    cmd_b = MagicMock()
    cmd_b.command = "dup"
    p._add_subcommand(cmd_a)  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    with pytest.raises(prompt.CommandAlreadyBoundError, match="dup"):
        p._add_subcommand(cmd_b)  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]


def test_set_cmdqueue_interactive_keeps_queue_as_is():
    """Interactive sessions must not get an auto-appended ``quit``."""
    p = _make_prompt()
    p.interactive = True
    p.set_cmdqueue(["a", "b"])
    assert list(p.cmdqueue) == ["a", "b"]
    assert isinstance(p.cmdqueue, prompt.CmdQueue)


def test_set_cmdqueue_non_interactive_appends_quit():
    """Non-interactive sessions terminate by appending ``quit``."""
    p = _make_prompt()
    p.interactive = False
    p.set_cmdqueue(["a", "b"])
    assert list(p.cmdqueue) == ["a", "b", "quit"]


def test_cmdloop_user_message_logs_error_then_quits(monkeypatch, caplog):
    """``UserMessage`` is logged at error level (non-debug path) and the loop continues."""
    p = _make_prompt()
    err = messages.NoRefhostsDefinedError()
    monkeypatch.setattr(
        "cmd.Cmd.cmdloop",
        MagicMock(side_effect=[err, prompt.QuitLoopError]),
    )
    with caplog.at_level("ERROR", logger="mtui.prompt"):
        p.cmdloop()
    assert any("No refhosts defined" in r.message for r in caplog.records)


def test_cmdloop_user_message_logs_traceback_in_debug(monkeypatch, caplog):
    """When debug is enabled, ``UserMessage`` is logged with a traceback."""
    p = _make_prompt()
    monkeypatch.setattr(
        "cmd.Cmd.cmdloop",
        MagicMock(
            side_effect=[messages.NoRefhostsDefinedError(), prompt.QuitLoopError]
        ),
    )
    with caplog.at_level("DEBUG", logger="mtui.prompt"):
        p.cmdloop()
    assert any(r.exc_info is not None for r in caplog.records)


def test_cmdloop_called_process_error_logs_and_continues(monkeypatch, caplog):
    """``subprocess.CalledProcessError`` follows the same path as ``UserMessage``."""
    p = _make_prompt()
    err = subprocess.CalledProcessError(1, ["false"])
    monkeypatch.setattr(
        "cmd.Cmd.cmdloop",
        MagicMock(side_effect=[err, prompt.QuitLoopError]),
    )
    with caplog.at_level("ERROR", logger="mtui.prompt"):
        p.cmdloop()
    assert any("false" in r.message or "1" in r.message for r in caplog.records)


def test_cmdloop_unexpected_error_logs_and_continues(monkeypatch, caplog):
    """Generic ``Exception`` is logged as 'Unexpected error' and the loop continues."""
    p = _make_prompt()
    monkeypatch.setattr(
        "cmd.Cmd.cmdloop",
        MagicMock(side_effect=[RuntimeError("kaboom"), prompt.QuitLoopError]),
    )
    with caplog.at_level("ERROR", logger="mtui.prompt"):
        p.cmdloop()
    assert any(
        "Unexpected error" in r.message and "kaboom" in r.message
        for r in caplog.records
    )


def test_cmdloop_unexpected_error_logs_traceback_in_debug(monkeypatch, caplog):
    """In debug mode the unexpected-error path uses ``logger.exception``."""
    p = _make_prompt()
    monkeypatch.setattr(
        "cmd.Cmd.cmdloop",
        MagicMock(side_effect=[RuntimeError("kaboom"), prompt.QuitLoopError]),
    )
    with caplog.at_level("DEBUG", logger="mtui.prompt"):
        p.cmdloop()
    assert any(
        r.exc_info is not None and "Unexpected error" in r.message
        for r in caplog.records
    )


def test_postcmd_short_circuits_for_null_test_report():
    """Until a real test report is loaded, ``postcmd`` must not touch the prompt."""
    p = _make_prompt()
    assert isinstance(p.metadata, NullTestReport)
    original_prompt = p.prompt
    assert p.postcmd(False, "noop") is False
    assert p.prompt == original_prompt


def test_postcmd_updates_prompt_when_metadata_loaded():
    """With a real test report, ``postcmd`` refreshes the prompt with the session."""
    p = _make_prompt()
    p.metadata = MagicMock()  # not a NullTestReport
    p.session = "sess1"
    p.postcmd(False, "noop")
    assert p.prompt == "mtui:sess1> "


def test_get_names_includes_registered_commands():
    """``get_names`` must surface ``do_X`` and ``help_X`` for every registered command."""
    p = _make_prompt()
    cmd = MagicMock()
    cmd.command = "alpha"
    p._add_subcommand(cmd)  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    names = p.get_names()
    assert "do_alpha" in names
    assert "help_alpha" in names


def test_do_handles_argsparse_failure(caplog):
    """``do_*`` swallows ``ArgsParseFailureError`` and does not invoke the command."""
    p = _make_prompt()
    cmd = MagicMock()
    cmd.command = "boom"
    cmd.parse_args.side_effect = ArgsParseFailureError()
    p._add_subcommand(cmd)  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    p.do_boom("--bad")
    cmd.assert_not_called()  # the command class itself must not be instantiated


def test_complete_logs_and_reraises(caplog):
    """``complete_*`` logs the exception then re-raises so readline sees it."""
    p = _make_prompt()
    cmd = MagicMock()
    cmd.command = "alpha"
    cmd.complete.side_effect = RuntimeError("comp-fail")
    p._add_subcommand(cmd)  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    with (
        caplog.at_level("ERROR", logger="mtui.prompt"),
        pytest.raises(RuntimeError, match="comp-fail"),
    ):
        p.complete_alpha("text", "line", 0, 1)
    assert any(r.exc_info is not None for r in caplog.records)


def test_getattr_unknown_attr_raises():
    """Unknown attributes that don't match the do_/help_/complete_ prefixes raise."""
    p = _make_prompt()
    with pytest.raises(AttributeError, match="no_such_thing"):
        p.no_such_thing  # noqa: B018  -- attribute access is the side effect under test


def test_getattr_unknown_command_raises():
    """do_/help_/complete_ for an unregistered command also raises ``AttributeError``."""
    p = _make_prompt()
    with pytest.raises(AttributeError):
        p.do_nonexistent  # noqa: B018


def test_emptyline_returns_false():
    """An empty input line must not stop the loop and must not repeat the last command."""
    p = _make_prompt()
    assert p.emptyline() is False


def test_set_prompt_auto_mode():
    """``config.auto`` (and not kernel) renders the ``mtui-auto`` prefix."""
    p = _make_prompt(auto=True, kernel=False)
    p.set_prompt("s1")
    assert p.prompt == "mtui-auto:s1> "


def test_set_prompt_kernel_mode():
    """``config.kernel`` overrides auto-mode and renders ``mtui-kernel``."""
    p = _make_prompt(auto=True, kernel=True)
    p.set_prompt(None)
    assert p.prompt == "mtui-kernel> "


def test_load_update_swaps_metadata_and_targets():
    """``load_update`` installs the new test report and resets the prompt."""
    p = _make_prompt()
    new_targets = MagicMock()
    new_tr = MagicMock(targets=new_targets)
    update = MagicMock()
    update.make_testreport.return_value = new_tr
    p.load_update(update, autoconnect=False)
    update.make_testreport.assert_called_once_with(p.config, False, p.interactive)
    assert p.metadata is new_tr
    assert p.targets is new_targets
    # ``set_prompt(None)`` resets the session marker to empty.
    assert p.prompt.endswith("> ")
    assert ":" not in p.prompt
