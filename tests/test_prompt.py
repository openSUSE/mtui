import pytest
from mtui import prompt
from unittest.mock import MagicMock

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
