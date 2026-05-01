"""A logging formatter that adds color to the output."""

import inspect
import logging

# ANSI color offsets (added to 30 to form the foreground color escape code).
# Positions matter: only RED..BLUE are referenced, but the indexes determine
# the resulting ANSI code (e.g. 30 + 1 = 31 = red).
(_, RED, GREEN, YELLOW, BLUE, *_) = list(range(8))

RESET_SEQ = "\033[0m"
COLOR_SEQ = "\033[1;{}m"

COLORS = {
    "WARNING": YELLOW,
    "INFO": GREEN,
    "DEBUG": BLUE,
    "CRITICAL": RED,
    "ERROR": RED,
}


class ColorFormatter(logging.Formatter):
    """A logging formatter that adds color to the output."""

    def __init__(self, msg) -> None:
        """Initializes the formatter.

        Args:
            msg: The format string to use.

        """
        logging.Formatter.__init__(self, msg)

    def formatColor(self, levelname: str) -> str:
        """Formats the log level name with ANSI color codes.

        Args:
            levelname: The name of the log level (e.g., 'INFO', 'DEBUG').

        Returns:
            The colorized log level name.

        """
        if levelname == "DEBUG":
            frame_info = self._find_caller_frame()
            if frame_info is None:
                module, function = "unknown", "unknown"
            else:
                frame, function = frame_info
                module = mo.__name__ if (mo := inspect.getmodule(frame)) else "unknown"
            return (
                "\033[2K"
                + COLOR_SEQ.format(30 + COLORS[levelname])
                + levelname.lower()
                + RESET_SEQ
                + f" [{module!s}:{function!s}]"
                + RESET_SEQ
            )
        return (
            "\033[2K"
            + COLOR_SEQ.format(30 + COLORS[levelname])
            + levelname.lower()
            + RESET_SEQ
        )

    @staticmethod
    def _find_caller_frame() -> tuple[object, str] | None:
        """Walks the stack until the first frame outside the logging machinery.

        Returns:
            A `(frame, function_name)` tuple for the first caller whose
            module is not `logging`, `mtui.colorlog`, or `contextlib`,
            or `None` if no such frame exists.

        """
        skip_modules = {"logging", "mtui.colorlog", "contextlib"}
        for frame_info in inspect.getouterframes(inspect.currentframe()):
            module = inspect.getmodule(frame_info.frame)
            module_name = module.__name__ if module else ""
            top_level = module_name.partition(".")[0]
            if module_name in skip_modules or top_level == "logging":
                continue
            return frame_info.frame, frame_info.function
        return None

    def format(self, record: logging.LogRecord) -> str:
        """Formats the log record.

        Args:
            record: The log record to format.

        Returns:
            The formatted log record as a string.

        """
        record.message = record.getMessage()
        if self._fmt and self._fmt.find("%(levelname)") >= 0:
            record.levelname = self.formatColor(record.levelname)

        return logging.Formatter.format(self, record)


def create_logger(name: str, level: str = "INFO") -> logging.Logger:
    """Creates a logger with a colorized output.

    Args:
        name: The name of the logger.
        level: The logging level.

    Returns:
        A configured `logging.Logger` instance.

    """
    out = logging.getLogger(name) if name else logging.getLogger()
    out.setLevel(level)
    handler = logging.StreamHandler()
    formatter = ColorFormatter("%(levelname)s: %(message)s")
    handler.setFormatter(formatter)
    out.addHandler(handler)
    return out
