from unittest.mock import MagicMock

from mtui import prompt


def test_command_prompt_init():
    """
    Test CommandPrompt initialization
    """
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
    """
    Test set_prompt
    """
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
    """
    Test CmdQueue
    """
    term = MagicMock()
    queue = prompt.CmdQueue(["test_cmd"], "mtui> ", term)

    cmd = queue.pop(0)

    assert cmd == "test_cmd"
    term.stdout.write.assert_called_with("mtui> test_cmd\n")


def test_dispatching():
    """
    Test command, help, and completion dispatching
    """
    config = MagicMock()
    log = MagicMock()
    sys = MagicMock()
    display_factory = MagicMock()

    p = prompt.CommandPrompt(config, log, sys, display_factory)

    mock_command = MagicMock()
    mock_command.command = "test_command"
    mock_argparser = MagicMock()
    mock_command.argparser.return_value = mock_argparser

    p._add_subcommand(mock_command)

    # Test do_
    do_method = getattr(p, "do_test_command")
    do_method("test_args")
    mock_command.parse_args.assert_called_with("test_args", sys)
    mock_command.return_value.assert_called_once()

    # Test help_
    help_method = getattr(p, "help_test_command")
    help_method()
    mock_argparser.print_help.assert_called_once()

    # Test complete_
    complete_method = getattr(p, "complete_test_command")
    complete_method("text", "line", 0, 1)
    mock_command.complete.assert_called_once()


def test_cmdloop_keyboard_interrupt(monkeypatch):
    """
    Test that cmdloop handles KeyboardInterrupt
    """
    config = MagicMock()
    log = MagicMock()
    sys = MagicMock()
    display_factory = MagicMock()

    p = prompt.CommandPrompt(config, log, sys, display_factory)

    # Raise KeyboardInterrupt on the first call, then QuitLoop
    monkeypatch.setattr(
        "cmd.Cmd.cmdloop", MagicMock(side_effect=[KeyboardInterrupt, prompt.QuitLoop])
    )

    p.cmdloop()

    assert p.interactive is True
    assert p.cmdqueue == []


def test_cmdloop_quit_loop(monkeypatch):
    """
    Test that cmdloop handles QuitLoop
    """
    config = MagicMock()
    log = MagicMock()
    sys = MagicMock()
    display_factory = MagicMock()

    p = prompt.CommandPrompt(config, log, sys, display_factory)

    monkeypatch.setattr("cmd.Cmd.cmdloop", MagicMock(side_effect=prompt.QuitLoop))

    p.cmdloop()  # Should exit cleanly
