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

class QadbReportCommentLengthWarning(UserMessage):
    def __str__(self):
        return 'comment strings > 100 chars are truncated by remote_qa_db_report.pl'
