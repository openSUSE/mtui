"""A logging formatter that adds color to the output."""

import inspect
import logging
from types import FrameType

from ...support.spinner import spinner_suspended
from .mode import colors_enabled

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

        Honours the runtime colour mode (see :mod:`mtui.colorctl`):
        when colour is disabled the level name is returned in its
        plain lowercased form, with the DEBUG-only module/function
        suffix preserved.

        Args:
            levelname: The name of the log level (e.g., 'INFO', 'DEBUG').

        Returns:
            The (optionally colorized) log level name.

        """
        if levelname == "DEBUG":
            frame_info = self._find_caller_frame()
            if frame_info is None:
                module, function = "unknown", "unknown"
            else:
                frame, function = frame_info
                module = frame.f_globals.get("__name__", "unknown")
            suffix = f" [{module!s}:{function!s}]"
            if not colors_enabled():
                return levelname.lower() + suffix
            return (
                COLOR_SEQ.format(30 + COLORS[levelname])
                + levelname.lower()
                + RESET_SEQ
                + suffix
                + RESET_SEQ
            )
        if not colors_enabled():
            return levelname.lower()
        return COLOR_SEQ.format(30 + COLORS[levelname]) + levelname.lower() + RESET_SEQ

    @staticmethod
    def _find_caller_frame() -> tuple[FrameType, str] | None:
        """Walks the stack until the first frame outside the logging machinery.

        Returns:
            A `(frame, function_name)` tuple for the first caller whose
            module is not `logging`, ``mtui.cli.colors.formatter``, or
            ``contextlib``, or `None` if no such frame exists.

        """
        # mutmut's mutation trampoline interposes a wrapper frame between
        # every function and its caller; treat it as logging machinery so
        # caller attribution stays on the real call site during mutation
        # runs. Inert in production -- the module is never loaded there.
        # Module names come from ``f_globals`` rather than
        # ``inspect.getmodule``: the latter maps filenames back to modules
        # and returns None when an import hook reports a different path
        # than the code object records (pytest's rewriter under mutmut).
        skip_modules = {
            "logging",
            "mtui.cli.colors.formatter",
            "contextlib",
            "mutmut.mutation.trampoline",
        }
        for frame_info in inspect.getouterframes(inspect.currentframe()):
            module_name = frame_info.frame.f_globals.get("__name__", "")
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
            # Substitute the colorized levelname only for the duration of
            # this handler's formatting: the record object is shared with
            # every other handler in the chain (a file handler, pytest's
            # caplog, ...), and leaking the ANSI-wrapped name corrupts
            # their view of the record.
            original_levelname = record.levelname
            record.levelname = self.formatColor(original_levelname)
            try:
                return logging.Formatter.format(self, record)
            finally:
                record.levelname = original_levelname

        return logging.Formatter.format(self, record)


class SpinnerAwareStreamHandler(logging.StreamHandler):
    """A stream handler that coordinates with a live TTY spinner.

    Every record is emitted inside :func:`spinner_suspended`: while a
    :class:`mtui.support.spinner.TtySpinner` is painting, the handler
    erases the current frame (``\\r`` + erase-to-end, homing the cursor
    to column 0), writes the record from a clean line, and lets the
    spinner repaint on its next tick. Without this, a record emitted
    mid-spin starts at the column where the frame write left the
    cursor, rendering with phantom leading padding (or, with colours
    off, appended straight after the frame text).

    When no spinner is active — notably off a TTY, where spinners never
    start — the wrapper adds nothing and the handler behaves exactly
    like a plain :class:`logging.StreamHandler`.
    """

    def emit(self, record: logging.LogRecord) -> None:
        """Emit the record with any live spinner frame erased first."""
        with spinner_suspended():
            super().emit(record)


def create_logger(name: str, level: str = "INFO") -> logging.Logger:
    """Creates a logger with a colorized, spinner-aware output.

    Args:
        name: The name of the logger.
        level: The logging level.

    Returns:
        A configured `logging.Logger` instance.

    """
    out = logging.getLogger(name) if name else logging.getLogger()
    out.setLevel(level)
    handler = SpinnerAwareStreamHandler()
    formatter = ColorFormatter("%(levelname)s: %(message)s")
    handler.setFormatter(formatter)
    out.addHandler(handler)
    return out
