from mtui import colorlog


def test_create_logger(capsys):
    """
    Test create_logger
    """
    logger = colorlog.create_logger("test_logger", "DEBUG")
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
