"""A set of classes for displaying messages to the user.

This module defines a collection of classes for displaying various types
of messages to the user, including errors, warnings, and informational
messages. These classes are used throughout the application to provide
consistent and informative feedback.
"""

from abc import ABC


class UserMessage(BaseException, ABC):
    """An abstract base class for messages to be displayed to the user."""

    message: str

    def __str__(self) -> str:
        return self.message

    def __eq__(self, x: object) -> bool:
        return str(self) == str(x)

    def __hash__(self) -> int:
        # Must mirror __eq__ (which compares on str(self)); hashing self
        # here would recurse forever.
        return hash(str(self))


class ErrorMessage(UserMessage, RuntimeError):  # noqa: N818
    """A program error message to be displayed to the user."""


class UserError(UserMessage, RuntimeError):
    """An error caused by improper usage of the program."""


class NoRefhostsDefinedError(UserError, ValueError):
    """Raised when an operation is requested without defined refhosts."""

    def __init__(self) -> None:
        self.message: str = "No refhosts defined"


class HostIsNotConnectedError(UserError, ValueError):
    """Raised when an operation is requested on a disconnected host."""

    def __init__(self, host) -> None:
        self.host = host
        self.message = f"Host {host!r} is not connected"


class SystemCommandNotFoundError(ErrorMessage):
    """Raised when a system command is not found."""

    _msg = "Command {0!r} not found"

    def __init__(self, command) -> None:
        self.command = command
        self.message = self._msg.format(command)


class SystemCommandError(ErrorMessage):
    """Raised when a system command fails."""

    _message = "Command failed."

    def __init__(self, rc, command) -> None:
        self.rc = rc
        self.command = command

    @property
    def message(self):
        """The error message."""
        return self._message + f" rc = {self.rc} Command: {self.command!r}"


class SvnCheckoutInterruptedError(ErrorMessage):
    """Raised when an SVN checkout is interrupted."""

    _msg = "Svn checkout of {0!r} interrupted"

    def __init__(self, uri) -> None:
        self.uri = uri
        self.message = self._msg.format(uri)


class SvnCheckoutFailed(ErrorMessage):
    """Raised when a test report template cannot be checked out."""

    _msg = "Test report for {0} does not exist.\nPlease check {1} for potential issues."

    def __init__(self, rrid, report_url: str) -> None:
        self.rrid = rrid
        self.report_url = report_url
        self.message = self._msg.format(rrid, report_url)


class TemplateDirNotUsableError(ErrorMessage):
    """Raised when the configured template_dir cannot be created."""

    _msg = (
        "Cannot create template directory {0}: {1}\n"
        "Please check the [mtui] template_dir option in your configuration."
    )

    def __init__(self, path, reason) -> None:
        self.path = path
        self.reason = reason
        self.message = self._msg.format(path, reason)


class ConnectingTargetFailedMessage(UserMessage):
    """A message for when connecting to a target fails."""

    def __init__(self, hostname, reason) -> None:
        self.hostname = hostname
        self.reason = reason

    def __str__(self) -> str:
        return f"connecting to {self.hostname} failed: {self.reason}"

    def __repr__(self) -> str:
        return f"<{self.__class__} {self.hostname!r}:{self.reason!r}>"


class MissingPackagesError(UserError):
    """Raised when packages are missing."""

    def __str__(self) -> str:
        return "Missing packages: TestReport not loaded and no -p given."


class TestReportNotLoadedError(UserError):
    """Raised when a test report is not loaded."""

    __test__ = False

    def __str__(self) -> str:
        return "TestReport not loaded"


class MetadataNotLoadedError(UserError):
    """Raised when a test report is not loaded."""

    def __str__(self) -> str:
        return "Metadata not found"


class TemplateNotLoadedError(UserError):
    """Raised when an RRID is not among the loaded templates."""

    def __init__(self, rrid: str) -> None:
        self.rrid = rrid
        self.message = f"Template not loaded: {rrid}"


class FanOutError(ErrorMessage):
    """Aggregate raised after a fan-out command fails on one or more templates.

    A fanned-out command (``scope = "fanout"``) keeps running across the
    remaining templates when one raises, collecting the per-template failures.
    After the loop, if any template failed, this aggregate is raised so the
    caller still observes the error while every template got its turn.
    """

    def __init__(self, failures: list[tuple[str, BaseException]]) -> None:
        self.failures = failures
        detail = "; ".join(f"{rrid}: {exc}" for rrid, exc in failures)
        rrids = ", ".join(rrid for rrid, _ in failures)
        self.message = f"fan-out failed on {rrids} ({detail})"


class MissingDoerError(ErrorMessage):
    """Base class for missing "doer" errors."""

    name: str  # Set by subclasses as a class variable

    def __init__(self, release) -> None:
        self.release = release

    @property
    def message(self):
        """The error message."""
        return f"Missing {self.name} for {self.release}"


class MissingPreparerError(MissingDoerError):
    """Raised when a preparer is missing."""

    name = "Preparer"


class MissingUpdaterError(MissingDoerError):
    """Raised when an updater is missing."""

    name = "Updater"


class MissingInstallerError(MissingDoerError):
    """Raised when an installer is missing."""

    name = "Installer"


class MissingUninstallerError(MissingDoerError):
    """Raised when an uninstaller is missing."""

    name = "Uninstaller"


class MissingDowngraderError(MissingDoerError):
    """Raised when a downgrader is missing."""

    name = "Downgrader"


class ReConnectFailed(ErrorMessage):
    """Raised when a reconnect attempt fails."""

    _msg = "Failed to re-connect to {}"

    def __init__(self, host) -> None:
        self.message = self._msg.format(host)


class ResultsMissingError(ErrorMessage):
    """missing results json file."""

    def __init__(self, test, arch) -> None:
        self.test = test
        self.arch = arch
        self.message = f"Test: {test} on arch: {arch} missing results.json file. Please restart it."
