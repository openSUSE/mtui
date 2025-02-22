import argparse
import sys


class ArgsParseFailure(RuntimeError):
    def __init__(self, status: int = 0) -> None:
        self.status = status
        super().__init__()


class ArgumentParser(argparse.ArgumentParser):
    def __init__(self, *a, **kw) -> None:
        self.sys = kw.get("sys_", sys)
        if "sys_" in kw:
            del kw["sys_"]

        super().__init__(*a, **kw)

    def print_help(self, file=None) -> None:
        """
        :param file: ignored, self.stdout is always used instead
        """
        # also takes care of default _HelpAction calling
        # print_help
        super().print_help(self.sys.stdout)

    def print_usage(self, file=None) -> None:
        """
        :param file: ignored, self.stdout is always used instead
        """
        super().print_usage(self.sys.stdout)

    def exit(self, status: int = 0, message: str | None = None) -> None:  # type: ignore
        # don't want to call sys.exit when calling -h or parsing
        # failed inside mtui - > so return's None instead of Never
        if message:
            self._print_message(message, self.sys.stderr)

        raise ArgsParseFailure(status)
