"""Tests for the mtui main entry point."""

import logging
from argparse import Namespace
from unittest.mock import MagicMock, patch

from mtui.cli.argparse import ArgsParseFailureError
from mtui.main import main, run_mtui


def test_main_args_parse_failure(monkeypatch):
    """Test main function when argument parsing fails."""
    monkeypatch.setattr("sys.argv", ["mtui", "--invalid"])

    with patch("mtui.main.get_parser") as mock_get_parser:
        # Mock parse_args to raise ArgsParseFailureError
        mock_parser = MagicMock()
        mock_get_parser.return_value = mock_parser

        # Setup the exception to be raised
        mock_parser.parse_args.side_effect = ArgsParseFailureError(status=2)

        # Call main and check result
        result = main()
        assert result == 2


def test_run_mtui_with_debug():
    """Test run_mtui with debug flag."""
    mock_config = MagicMock()
    mock_logger = MagicMock()

    # Mock the detect_system function to return consistent values
    with patch("mtui.main.detect_system") as mock_detect_system:
        mock_detect_system.return_value = ("ubuntu", "20.04", "5.4.0")

        # Mock CommandPrompt to return a mock instance
        with patch("mtui.main.CommandPrompt") as mock_command_prompt_class:
            mock_prompt_instance = MagicMock()
            mock_command_prompt_class.return_value = mock_prompt_instance

            # Test with debug flag
            mock_args = Namespace(debug=True, update=None, sut=None)

            # Call run_mtui and check result
            result = run_mtui(mock_config, mock_logger, mock_args)
            assert result == 0

            # Verify logger level was set to debug
            mock_logger.setLevel.assert_called_once_with(level=logging.DEBUG)


def test_run_mtui_quits_when_explicit_update_not_loaded():
    """An explicit -a update that fails to load exits 1 without a session."""
    from mtui.test_reports.null_report import NullTestReport

    mock_config = MagicMock()
    mock_logger = MagicMock()

    with (
        patch("mtui.main.detect_system", return_value=("sles", "15", "6")),
        patch("mtui.main.Prompter"),
        patch("mtui.main.CommandPrompt") as cp,
    ):
        prompt = cp.return_value
        # Failed checkout falls back to a NullTestReport.
        prompt.metadata = NullTestReport(mock_config)
        update = MagicMock()
        update.kind = "auto"
        args = Namespace(debug=False, update=update, sut=None)

        result = run_mtui(mock_config, mock_logger, args)

    assert result == 1
    prompt.cmdloop.assert_not_called()


def test_run_mtui_starts_session_when_update_loads():
    """A successfully loaded explicit update proceeds to the command loop."""
    mock_config = MagicMock()
    mock_logger = MagicMock()

    with (
        patch("mtui.main.detect_system", return_value=("sles", "15", "6")),
        patch("mtui.main.Prompter"),
        patch("mtui.main.CommandPrompt") as cp,
    ):
        prompt = cp.return_value
        prompt.metadata = MagicMock()  # a real (non-Null) test report
        update = MagicMock()
        update.kind = "auto"
        args = Namespace(debug=False, update=update, sut=None)

        result = run_mtui(mock_config, mock_logger, args)

    assert result == 0
    prompt.cmdloop.assert_called_once()
