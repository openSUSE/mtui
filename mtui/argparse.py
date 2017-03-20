

import argparse
import sys


class ArgsParseFailure(RuntimeError):

    def __init__(self, status=0):
        self.status = status
        super(ArgsParseFailure, self).__init__()


class ArgumentParser(argparse.ArgumentParser):

    def __init__(self, *a, **kw):
        self.sys = kw.get('sys_', sys)
        if 'sys_' in kw:
            del kw['sys_']

        super(ArgumentParser, self).__init__(*a, **kw)

    def print_help(self, file=None):
        """
        :param file: ignored, self.stdout is always used instead
        """
        # also takes care of default _HelpAction calling
        # print_help
        super(ArgumentParser, self).print_help(self.sys.stdout)

    def print_usage(self, file=None):
        """
        :param file: ignored, self.stdout is always used instead
        """
        super(ArgumentParser, self).print_usage(self.sys.stdout)

    def exit(self, status=0, message=None):
        # don't want to call sys.exit when calling -h or parsing
        # failed inside mtui
        if message:
            self._print_message(message, self.sys.stderr)

        raise ArgsParseFailure(status)
