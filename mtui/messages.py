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
        return hash(self)


class ErrorMessage(UserMessage, RuntimeError):  # noqa: N818
    """A program error message to be displayed to the user."""


class UserError(UserMessage, RuntimeError):
    """An error caused by improper usage of the program."""


class DeprecationMessage(UserMessage):
    """A message for deprecated features."""


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


class UnexpectedlyFastCleanExitFromXdgOpen(UserMessage):
    """A message for when `xdg-open` exits suspiciously fast."""

    message = "xdg-open finished successfully but suspiciously too fast"


class SvnCheckoutInterruptedError(ErrorMessage):
    """Raised when an SVN checkout is interrupted."""

    _msg = "Svn checkout of {0!r} interrupted"

    def __init__(self, uri) -> None:
        self.uri = uri
        self.message = self._msg.format(uri)


class SvnCheckoutFailed(ErrorMessage):
    """Raised when an SVN checkout fails."""

    _msg = "Svn checkout of {0!r} Failed\n Please check {1!s}"

    def __init__(self, uri, f_url: str) -> None:
        self.uri = uri
        self.f_url = f_url
        self.message = self._msg.format(uri, f_url)


class QadbReportCommentLengthWarning(UserMessage):
    """A warning about comment length limitations."""

    def __str__(self) -> str:
        return "comment strings > 100 chars are truncated by remote_qa_db_report.pl"


class ConnectingTargetFailedMessage(UserMessage):
    """A message for when connecting to a target fails."""

    def __init__(self, hostname, reason) -> None:
        self.hostname = hostname
        self.reason = reason

    def __str__(self) -> str:
        return f"connecting to {self.hostname} failed: {self.reason}"

    def __repr__(self) -> str:
        return f"<{self.__class__} {self.hostname!r}:{self.reason!r}>"


class ConnectingToMessage(UserMessage):
    """A message for when connecting to a target."""

    def __init__(self, hostname) -> None:
        self.hostname = hostname

    def __str__(self) -> str:
        return f"connecting to {self.hostname}"


class MissingPackagesError(UserError):
    """Raised when packages are missing."""

    def __str__(self) -> str:
        return "Missing packages: TestReport not loaded and no -p given."


class TestReportNotLoadedError(UserError):
    """Raised when a test report is not loaded."""

    def __str__(self) -> str:
        return "TestReport not loaded"


class MetadataNotLoadedError(UserError):
    """Raised when a test report is not loaded."""

    def __str__(self) -> str:
        return "Metadata not found"


class FailedToWriteScriptResult(UserMessage):
    """A message for when writing a script result fails."""

    def __init__(self, path, reason) -> None:
        self.path = path
        self.reason = reason

    def __str__(self) -> str:
        return f"failed to write script output to {self.path}: {self.reason}"


class StartingCompareScriptError(UserMessage):
    """A message for when starting a compare script fails."""

    def __init__(self, reason, argv) -> None:
        self.reason = reason
        self.argv = argv

    def __str__(self) -> str:
        return f"Starting compare script {self.argv!r} failed: {self.reason}"


class CompareScriptError(UserMessage):
    """Base class for compare script errors."""

    def __init__(self, argv, stdout, stderr, rc) -> None:
        self.argv = argv
        self.stderr = stderr
        self.stdout = stdout
        self.rc = rc

    def __str__(self):
        raise NotImplementedError


class CompareScriptFailedError(CompareScriptError):
    """A message for when a compare script fails."""

    def __str__(self) -> str:
        return (
            f"Compare script {self.argv!r} failed: rc = {self.rc} err:\n{self.stderr}"
        )


class CompareScriptCrashedError(CompareScriptError):
    """A message for when a compare script crashes."""

    def __str__(self) -> str:
        return f"Compare script {self.argv!r} crashed:\n{self.stderr}"


class LocationChangedMessage(UserMessage):
    """A message for when the location changes."""

    def __init__(self, old, new) -> None:
        self.old = old
        self.new = new

    @property
    def message(self):
        """The message."""
        return f"changed location from {self.old!r} to {self.new!r}"


class PackageRevisionHasntChangedWarning(UserMessage):
    """A warning for when a package revision has not changed."""

    _msg = (
        "Revision of package {0!r} hasn't changed, "
        + "it's most likely already updated. skipping."
    )

    def __init__(self, package) -> None:
        self.message = self._msg.format(package)


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


class InvalidLocationError(UserError):
    """Raised when an invalid location is specified."""

    _msg = "Invalid location {0!r}. Available locations: {1}"

    def __init__(self, requested, available) -> None:
        self.requested = requested
        self.available = available

        self.message = self._msg.format(requested, ", ".join(available))


class ReConnectFailed(ErrorMessage):
    """Raised when a reconnect attempt fails."""

    _msg = "Failed to re-connect to {}"

    def __init__(self, host) -> None:
        self.message = self._msg.format(host)


class RepositoryError(ErrorMessage):
    """failed to read IBS Repository."""

    def __init__(self, repo) -> None:
        self.repo = repo
        self.message = f"Repository empty {repo}"


class openQAError(ErrorMessage):
    """openQA related Errors."""

    def __init__(self) -> None:
        self.message = "Something wrong with openQA connection"


class ResultsMissingError(ErrorMessage):
    """missing results json file."""

    def __init__(self, test, arch) -> None:
        self.test = test
        self.arch = arch
        self.message = f"Test: {test} on arch: {arch} missing results.json file. Please restart it."


class SMELTError(ErrorMessage):
    """SMELT related Errors."""

    def __init__(self) -> None:
        self.message = "Sommething wrong with SMELT connection"


class SVNError(ErrorMessage):
    """SVN related Errors."""

    def __init__(self, cmd) -> None:
        self.cmd = cmd
        self.message: str = f"SVN {cmd} command failed"
