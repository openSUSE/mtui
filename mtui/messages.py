from abc import ABCMeta
from abc import abstractmethod

class UserMessage(object):
    __metaclass__ = ABCMeta
    def __str__(self):
        return self.message

    @abstractmethod
    def __str__(self):
        pass

    def __eq__(self, x):
        return str(self) == str(x)

class UserError(UserMessage, RuntimeError):
    pass

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
