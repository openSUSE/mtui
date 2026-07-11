import pytest

from mtui.cli import colors


def test_create_logger(capsys):
    """Test create_logger."""
    saved = colors.get_mode()
    colors.set_mode("always")
    try:
        logger = colors.create_logger("test_logger", "DEBUG")
        logger.info("test info message")
        logger.debug("test debug message")
        logger.warning("test warning message")
        logger.error("test error message")
        logger.critical("test critical message")

        captured = capsys.readouterr()

        # Check for colorized output
        assert "\033[1;32m" in captured.err  # Green for INFO
        assert "\033[1;34m" in captured.err  # Blue for DEBUG
        assert "\033[1;33m" in captured.err  # Yellow for WARNING
        assert "\033[1;31m" in captured.err  # Red for ERROR and CRITICAL

        # Check for log messages
        assert "test info message" in captured.err
        assert "test debug message" in captured.err
        assert "test warning message" in captured.err
        assert "test error message" in captured.err
        assert "test critical message" in captured.err

        # Check for debug message format
        assert "[tests.test_colorlog:test_create_logger]" in captured.err
    finally:
        # create_logger attaches a fresh handler to the process-global
        # "test_logger" on every call; drop it so repeated in-process
        # pytest runs (mutmut) don't accumulate handlers bound to closed
        # capture streams.
        logger.handlers.clear()
        colors.set_mode(saved)


def test_format_does_not_mutate_record_for_downstream_handlers():
    """format() must not leak the colorized levelname into the shared record.

    Every handler on a logger receives the same LogRecord object; if
    ColorFormatter rewrote record.levelname in place, each later handler
    (a file handler, pytest's caplog) would see the ANSI-wrapped name.
    """
    import logging

    from mtui.cli.colors.formatter import ColorFormatter

    saved = colors.get_mode()
    colors.set_mode("always")
    try:
        formatter = ColorFormatter("%(levelname)s: %(message)s")
        warning = logging.LogRecord(
            "mtui.test", logging.WARNING, __file__, 1, "boom", None, None
        )
        out = formatter.format(warning)
        assert "\033[" in out
        assert warning.levelname == "WARNING"

        # The DEBUG path takes the caller-attribution branch as well.
        debug = logging.LogRecord(
            "mtui.test", logging.DEBUG, __file__, 1, "detail", None, None
        )
        out = formatter.format(debug)
        assert "\033[" in out
        assert debug.levelname == "DEBUG"
    finally:
        colors.set_mode(saved)


def test_format_restores_levelname_when_formatter_raises():
    """The restore must happen even if the wrapped format() call raises.

    format() colorizes record.levelname, then calls
    logging.Formatter.format() inside a try/finally so the original
    levelname is put back no matter what -- otherwise an exception
    partway through formatting (a bad field in the format string, a
    broken %-arg, ...) would leave the shared record permanently
    corrupted for every other handler on the logger.
    """
    import logging

    from mtui.cli.colors.formatter import ColorFormatter

    saved = colors.get_mode()
    colors.set_mode("always")
    try:
        # %(nonexistent)s does not exist on LogRecord, so
        # logging.PercentStyle.format() raises ValueError while
        # logging.Formatter.format() is still running -- after
        # formatColor() has already substituted the colorized name in.
        formatter = ColorFormatter("%(nonexistent)s %(levelname)s: %(message)s")
        record = logging.LogRecord(
            "mtui.test", logging.WARNING, __file__, 1, "boom", None, None
        )
        with pytest.raises(ValueError, match="nonexistent"):
            formatter.format(record)
        assert record.levelname == "WARNING"
    finally:
        colors.set_mode(saved)
