from abc import ABCMeta
from abc import abstractmethod

class UserMessage(object):
    """
    Message to be displayed to the user
    """
    __metaclass__ = ABCMeta
    def __str__(self):
        return self.message

    @abstractmethod
    def __str__(self):
        pass

    def __eq__(self, x):
        return str(self) == str(x)

class ErrorMessage(UserMessage, RuntimeError):
    """
    Program error message to be displayed to the user
    """

class UserError(UserMessage, RuntimeError):
    """
    Error, caused by improper usage of the program, to be displayed to
    the user
    """

class SystemCommandError(UserMessage):
    def __init__(self, rc, command):
        self.rc = rc
        self.command = command

    @property
    def message(self):
        return self._message + " rc = {0} Command: {1!r}".format(
            self.rc,
            self.command
        )

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
