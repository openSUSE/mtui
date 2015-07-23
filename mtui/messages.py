from abc import ABCMeta
from mtui.five import with_metaclass

class UserMessage(with_metaclass(ABCMeta, object)):
    """
    Message to be displayed to the user
    """
    def __str__(self):
        return self.message

    def __eq__(self, x):
        return str(self) == str(x)

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

class ListPackagesAllHost(DeprecationMessage):
    message = "Perhaps you meant to run just `list_packages`." \
            + " Argument `all` is no longer accepted."

class HostIsNotConnectedError(UserError, ValueError):
    """
    Thrown when user requests an operation to be performed on a host
    that is not connected.
    """
    def __init__(self, host):
        self.host = host
        self.message = "Host {0!r} is not connected".format(host)

class SystemCommandNotFoundError(ErrorMessage):
    _msg = "Command {0!r} not found"

    def __init__(self, command):
        self.command = command
        self.message = self._msg.format(command)

class SystemCommandError(ErrorMessage):
    _message = "Command failed."

    def __init__(self, rc, command):
        self.rc = rc
        self.command = command

    @property
    def message(self):
        return self._message + " rc = {0} Command: {1!r}".format(
            self.rc,
            self.command
        )

class UnexpectedlyFastCleanExitFromXdgOpen(UserMessage):
    message = "xdg-open finished successfully but suspiciously too fast"

class SvnCheckoutInterruptedError(ErrorMessage):
    _msg = "Svn checkout of {0!r} interrupted"

    def __init__(self, uri):
        self.uri = uri
        self.message = self._msg.format(uri)

class QadbReportCommentLengthWarning(UserMessage):
    def __str__(self):
        return 'comment strings > 100 chars are truncated by remote_qa_db_report.pl'

class ConnectingTargetFailedMessage(UserMessage):
    def __init__(self, hostname, reason):
        self.hostname = hostname
        self.reason = reason

    def __str__(self):
        return 'connecting to {0} failed: {1}'.format(
            self.hostname, self.reason
        )

    def __repr__(self):
        return '<{0} {1!r}:{2!r}>'.format(
            self.__class__,
            self.hostname,
            self.reason
        )

class ConnectingToMessage(UserMessage):
    def __init__(self, hostname):
        self.hostname = hostname

    def __str__(self):
        return 'connecting to {0}'.format(self.hostname)

class MissingPackagesError(UserError):
    def __str__(self):
        return "Missing packages: TestReport not loaded and no -p given."

class TestReportNotLoadedError(UserError):
    def __str__(self):
        return 'TestReport not loaded'

class FailedToWriteScriptResult(UserMessage):
    def __init__(self, path, reason):
        self.path = path
        self.reason = reason

    def __str__(self):
        return "failed to write script output to {0}: {1}".format(
            self.path,
            self.reason,
        )

class StartingCompareScriptError(UserMessage):
    def __init__(self, reason, argv):
        self.reason = reason
        self.argv = argv

    def __str__(self):
        return "Starting compare script {0!r} failed: {1}".format(
            self.argv,
            self.reason
        )

class CompareScriptError(UserMessage):
    def __init__(self, argv, stdout, stderr, rc):
        self.argv = argv
        self.stderr = stderr
        self.stdout = stdout
        self.rc = rc

    def __str__(self):
        raise NotImplementedError

class CompareScriptFailed(CompareScriptError):
    def __str__(self):
        return "Compare script {0!r} failed: rc = {1} err:\n{2}".format(
            self.argv,
            self.rc,
            self.stderr,
        )

class CompareScriptCrashed(CompareScriptError):
    def __str__(self):
        return "Compare script {0!r} crashed:\n{1}".format(
            self.argv,
            self.stderr,
        )

class FailedToExtractSrcRPM(SystemCommandError):
    _message = "Failed to extract source rpm."

class SrcRPMExtractedMessage(UserMessage):
    def __init__(self, dir):
        self.dir = dir

    def __str__(self):
        return 'Extracted source rpm to {0}'.format(self.dir)

class LocationChangedMessage(UserMessage):
    def __init__(self, old, new):
        self.old = old
        self.new = new

    @property
    def message(self):
        return 'changed location from {0!r} to {1!r}'.format(
            self.old, self.new
        )

class PackageRevisionHasntChangedWarning(UserMessage):
    _msg = "Revision of package {0!r} hasn't changed, " \
         + "it's most likely already updated. skipping."

    def __init__(self, package):
        self.message =  self._msg.format(package)

class MissingDoerError(ErrorMessage):
    def __init__(self, release):
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

    def __init__(self, requested, available):
        self.requested = requested
        self.available = available

        self.message = self._msg.format(requested, ", ".join(available))

class InvalidOBSDistURL(ErrorMessage):
    def __init__(self, url):
        self.message = "Invalid OBS DistURL: {0!r}".format(url)
