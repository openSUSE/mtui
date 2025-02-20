from abc import ABC


class UserMessage(BaseException, ABC):
    """
    Message to be displayed to the user
    """

    def __str__(self) -> str:
        return self.message  # type: ignore

    def __eq__(self, x: object) -> bool:
        return str(self) == str(x)

    @classmethod
    def __hash__(cls) -> int:  # type: ignore
        return hash(cls)


class ErrorMessage(UserMessage, RuntimeError):
    """
    Program error message to be displayed to the user
    """


class UserError(UserMessage, RuntimeError):
    """
    Error, caused by improper usage of the program,
    to be displayed to the user
    """


class DeprecationMessage(UserMessage):
    pass


class NoRefhostsDefinedError(UserError, ValueError):
    """
    Thrown when user requests an operation without defined refhosts
    """

    def __init__(self) -> None:
        self.message: str = "No refhosts defined"


class HostIsNotConnectedError(UserError, ValueError):
    """
    Thrown when user requests an operation to be performed on a host
    that is not connected.
    """

    def __init__(self, host) -> None:
        self.host = host
        self.message = "Host {0!r} is not connected".format(host)


class SystemCommandNotFoundError(ErrorMessage):
    _msg = "Command {0!r} not found"

    def __init__(self, command) -> None:
        self.command = command
        self.message = self._msg.format(command)


class SystemCommandError(ErrorMessage):
    _message = "Command failed."

    def __init__(self, rc, command) -> None:
        self.rc = rc
        self.command = command

    @property
    def message(self):
        return self._message + " rc = {0} Command: {1!r}".format(self.rc, self.command)


class UnexpectedlyFastCleanExitFromXdgOpen(UserMessage):
    message = "xdg-open finished successfully but suspiciously too fast"


class SvnCheckoutInterruptedError(ErrorMessage):
    _msg = "Svn checkout of {0!r} interrupted"

    def __init__(self, uri) -> None:
        self.uri = uri
        self.message = self._msg.format(uri)


class SvnCheckoutFailed(ErrorMessage):
    _msg = "Svn checkout of {0!r} Failed\n Please check {1!s}"

    def __init__(self, uri, f_url: str) -> None:
        self.uri = uri
        self.f_url = f_url
        self.message = self._msg.format(uri, f_url)


class QadbReportCommentLengthWarning(UserMessage):
    def __str__(self) -> str:
        return "comment strings > 100 chars are truncated by remote_qa_db_report.pl"


class ConnectingTargetFailedMessage(UserMessage):
    def __init__(self, hostname, reason) -> None:
        self.hostname = hostname
        self.reason = reason

    def __str__(self) -> str:
        return "connecting to {0} failed: {1}".format(self.hostname, self.reason)

    def __repr__(self) -> str:
        return "<{0} {1!r}:{2!r}>".format(self.__class__, self.hostname, self.reason)


class ConnectingToMessage(UserMessage):
    def __init__(self, hostname) -> None:
        self.hostname = hostname

    def __str__(self) -> str:
        return "connecting to {0}".format(self.hostname)


class MissingPackagesError(UserError):
    def __str__(self) -> str:
        return "Missing packages: TestReport not loaded and no -p given."


class TestReportNotLoadedError(UserError):
    def __str__(self) -> str:
        return "TestReport not loaded"


class FailedToWriteScriptResult(UserMessage):
    def __init__(self, path, reason) -> None:
        self.path = path
        self.reason = reason

    def __str__(self) -> str:
        return "failed to write script output to {0}: {1}".format(
            self.path, self.reason
        )


class StartingCompareScriptError(UserMessage):
    def __init__(self, reason, argv) -> None:
        self.reason = reason
        self.argv = argv

    def __str__(self) -> str:
        return "Starting compare script {0!r} failed: {1}".format(
            self.argv, self.reason
        )


class CompareScriptError(UserMessage):
    def __init__(self, argv, stdout, stderr, rc) -> None:
        self.argv = argv
        self.stderr = stderr
        self.stdout = stdout
        self.rc = rc

    def __str__(self):
        raise NotImplementedError


class CompareScriptFailed(CompareScriptError):
    def __str__(self) -> str:
        return "Compare script {0!r} failed: rc = {1} err:\n{2}".format(
            self.argv, self.rc, self.stderr
        )


class CompareScriptCrashed(CompareScriptError):
    def __str__(self) -> str:
        return "Compare script {0!r} crashed:\n{1}".format(self.argv, self.stderr)


class LocationChangedMessage(UserMessage):
    def __init__(self, old, new) -> None:
        self.old = old
        self.new = new

    @property
    def message(self):
        return "changed location from {0!r} to {1!r}".format(self.old, self.new)


class PackageRevisionHasntChangedWarning(UserMessage):
    _msg = (
        "Revision of package {0!r} hasn't changed, "
        + "it's most likely already updated. skipping."
    )

    def __init__(self, package) -> None:
        self.message = self._msg.format(package)


class MissingDoerError(ErrorMessage):
    def __init__(self, release) -> None:
        self.release = release

    @property
    def message(self):
        return "Missing {0} for {1}".format(self.name, self.release)


class MissingPreparerError(MissingDoerError):
    name = "Preparer"


class MissingUpdaterError(MissingDoerError):
    name = "Updater"


class MissingInstallerError(MissingDoerError):
    name = "Installer"


class MissingUninstallerError(MissingDoerError):
    name = "Uninstaller"


class MissingDowngraderError(MissingDoerError):
    name = "Downgrader"


class InvalidLocationError(UserError):
    _msg = "Invalid location {0!r}. Available locations: {1}"

    def __init__(self, requested, available) -> None:
        self.requested = requested
        self.available = available

        self.message = self._msg.format(requested, ", ".join(available))


class ReConnectFailed(ErrorMessage):
    _msg = "Failed to re-connect to {}"

    def __init__(self, host) -> None:
        self.message = self._msg.format(host)


class RepositoryError(ErrorMessage):
    """failed to read IBS Repository"""

    def __init__(self, repo) -> None:
        self.repo = repo
        self.message = "Repository empty {}".format(repo)


class openQAError(ErrorMessage):
    """openQA related Errors"""

    def __init__(self) -> None:
        self.message = "Something wrong with openQA connection"


class ResultsMissingError(ErrorMessage):
    """missing results json file"""

    def __init__(self, test, arch) -> None:
        self.test = test
        self.arch = arch
        self.message = f"Test: {test} on arch: {arch} missing results.json file. Please restart it."


class SMELTError(ErrorMessage):
    def __init__(self) -> None:
        self.message = "Sommething wrong with SMELT connection"


class SVNError(ErrorMessage):
    """SVN related Errors"""

    def __init__(self, cmd) -> None:
        self.cmd = cmd
        self.message: str = "SVN {} command failed".format(cmd)
