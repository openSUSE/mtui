#
# mtui command line prompt
#

import cmd
from collections.abc import Callable
from logging import getLogger
from pathlib import Path
import readline
import subprocess
from traceback import format_exc
from typing import Any, Type

import mtui.notification as notification

from . import commands, messages
from .argparse import ArgsParseFailure
from .commands import Command
from .template.nulltestreport import NullTestReport

logger = getLogger("mtui.prompt")


class QuitLoop(RuntimeError):
    pass


class CmdQueue(list):
    """
    Prerun support.

    Echos prompt with the command that's being popped (and about to be
    executed
    """

    def __init__(self, iterable, prompt, term) -> None:
        self.prompt = prompt
        self.term = term
        list.__init__(self, iterable)

    def pop(self, i) -> Any:  # type: ignore
        val = list.pop(self, i)
        self.echo_prompt(val)
        return val

    def echo_prompt(self, val) -> None:
        self.term.stdout.write("{0}{1}\n".format(self.prompt, val))


class CommandAlreadyBoundError(RuntimeError):
    pass


class CommandPrompt(cmd.Cmd):
    # TODO: It's worth considering to remove the inherit of cmd.Cmd and
    # just copy some of it's needed functionality, because
    #
    # 1. cmd.Cmd is not written in unit test friendly way.
    #
    # 2. cmd.Cmd.cmdloop() has to be wrapped or "clever" hacks
    #    (CmdQueue) devised in order to implement some features and
    #    tests (KeyboardInterrupt, prerun, stepping the loop one input
    #    by one) and the whole logic appears more complicated than
    #    it needs to be.
    #
    # 3. using methods as commands is quite simple but wrong way to do
    #    that and handling classes is hacked into the function system.
    #
    # 4. L{cmd.Cmd} does not inherit L{object}, therefore we can't use
    #    property accessor decorators and super
    #
    # Note: it might be possible to choose from several existing CLI
    # frameworks. Eg. cement. Maybe there's something in twisted, which
    # would be great if it could replace the ssh layer as well.

    def __init__(self, config, log, sys, display_factory) -> None:
        self.sys = sys

        super().__init__(stdout=self.sys.stdout, stdin=self.sys.stdin)
        self.interactive: bool = True
        self.display = display_factory(self.sys.stdout)
        self.metadata = NullTestReport(config)
        self.targets = self.metadata.targets
        """
        alias to ease refactoring
        """

        self.homedir = Path("~").expanduser()
        self.config = config
        self.log = log
        self.datadir = self.config.datadir

        readline.set_completer_delims(r'`!@#$%^&*()=+[{]}\|;",<>? ')

        self._read_history()

        self.commands: dict[str, Type[Command]] = {}

        # register commands
        for x in commands.cmd_list:
            self._add_subcommand(getattr(commands, x))

        # self.stdout is used by cmd.Cmd
        self.stdout = self.sys.stdout
        # support commands with dashes in them
        self.identchars += "-"
        # set default prompt, when is loadet template is overrriden. So wisible
        # only wheen mtui is started without param.
        self.prompt: str = "mtui-empty>"

    def notify_user(self, msg: str, class_=""):
        notification.display("MTUI", msg, class_)

    def println(self, msg="", eol="\n") -> None:
        self.stdout.write(msg + eol)

    def _read_history(self) -> None:
        try:
            readline.read_history_file("{!s}/.mtui_history".format(self.homedir))
        except IOError as e:
            logger.debug("failed to open history file: {!s}".format(e))

    def _add_subcommand(self, cmd: Type[Command]) -> None:
        if cmd.command in self.commands:
            raise CommandAlreadyBoundError(cmd.command)
        self.commands[cmd.command] = cmd

    def set_cmdqueue(self, queue: list[str]) -> None:
        q = queue[:]
        if not self.interactive:
            q.append("quit")

        self.cmdqueue = CmdQueue(q, self.prompt, self.sys)

    def cmdloop(self, intro=None) -> None:
        """
        Customized cmd.Cmd.cmdloop so it handles Ctrl-C and prerun
        """
        while True:
            try:
                super().cmdloop(intro=intro)
            except KeyboardInterrupt:
                # Drop to interactive mode.
                # This takes effect only if we are in prerun
                self.interactive = True
                self.cmdqueue = []
                # make the new prompt to be printed on new line
                self.println()
            except QuitLoop:
                return
            except (messages.UserMessage, subprocess.CalledProcessError) as e:
                logger.error(e)
                logger.debug(format_exc())
            except Exception:
                logger.error(format_exc())

    def postcmd(self, stop: bool, line: str) -> bool:
        if isinstance(self.metadata, NullTestReport):
            return stop
        else:
            self.set_prompt(session=self.__dict__.get("session", None))
            return stop

    def get_names(self) -> list[str]:
        names = super().get_names()
        names += ["do_" + x for x in self.commands.keys()]
        names += ["help_" + x for x in self.commands.keys()]
        return names

    def __getattr__(self, x: str) -> Callable:
        if x.startswith("help_"):
            y = x.replace("help_", "", 1)
            if y in self.commands:
                c = self.commands[y]

                def help() -> None:
                    c.argparser(self.sys).print_help()

                return help

        elif x.startswith("do_"):
            y = x.replace("do_", "", 1)
            if y in self.commands:
                c = self.commands[y]

                def do(arg) -> None:
                    try:
                        args = c.parse_args(arg, self.sys)
                    except ArgsParseFailure:
                        return
                    c(args, self.config, self.sys, self)()

                return do

        elif x.startswith("complete_"):
            y = x.replace("complete_", "", 1)
            if y in self.commands:
                c = self.commands[y]

                def complete(*args, **kw):
                    try:
                        return c.complete(
                            {
                                "hosts": self.targets.select(),
                                "metadata": self.metadata,
                                "config": self.config,
                            },
                            *args,
                            **kw,
                        )
                    except Exception as e:
                        logger.error(e)
                        logger.debug(format_exc())
                        raise e

                return complete

        raise AttributeError(str(x))

    def emptyline(self) -> bool:
        return False

    def set_prompt(self, session: str | None = None) -> None:
        self.session = session
        session = ":" + str(session) if session else ""
        mode = "mtui"
        if self.config.auto and not self.config.kernel:
            mode += "-auto"
        elif self.config.kernel:
            mode += "-kernel"
        self.prompt = f"{mode}{session}> "

    def load_update(self, update, autoconnect: bool) -> None:
        tr = update.make_testreport(self.config, autoconnect=autoconnect)
        self.metadata = tr
        self.targets = tr.targets
        self.set_prompt(None)
