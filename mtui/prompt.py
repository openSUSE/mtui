# -*- coding: utf-8 -*-
#
# mtui command line prompt
#

import os
import cmd
import readline
import subprocess

from logging import getLogger

import mtui.notification as notification

from traceback import format_exc

from .argparse import ArgsParseFailure

from mtui import commands
from mtui import messages
from mtui.template.nulltestreport import NullTestReport
from qamlib.utils import ensure_dir_exists
from qamlib.utils import timestamp
from mtui.utils import prompt_user

logger = getLogger("mtui.prompt")


class QuitLoop(RuntimeError):
    pass


class CmdQueue(list):

    """
    Prerun support.

    Echos prompt with the command that's being popped (and about to be
    executed
    """

    def __init__(self, iterable, prompt, term):
        self.prompt = prompt
        self.term = term
        list.__init__(self, iterable)

    def pop(self, i):
        val = list.pop(self, i)
        self.echo_prompt(val)
        return val

    def echo_prompt(self, val):
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

    def __init__(self, config, log, sys, display_factory):
        self.set_prompt()
        self.sys = sys

        cmd.Cmd.__init__(self, stdout=self.sys.stdout, stdin=self.sys.stdin)
        self.interactive = True
        self.display = display_factory(self.sys.stdout)
        self.metadata = NullTestReport(config)
        self.targets = self.metadata.targets
        """
        alias to ease refactoring
        """

        self.homedir = os.path.expanduser('~')
        self.config = config
        self.log = log
        self.datadir = self.config.datadir

        self.testopia = None

        readline.set_completer_delims(r'`!@#$%^&*()=+[{]}\|;",<>? ')

        self._read_history()

        self.commands = {}

        # register commands
        for x in commands.cmd_list:
            self._add_subcommand(getattr(commands, x))

        self.stdout = self.sys.stdout
        # self.stdout is used by cmd.Cmd
        self.identchars += "-"
        # support commands with dashes in them

    def notify_user(self, msg, class_=None):
        notification.display("MTUI", msg, class_)

    def println(self, msg="", eol="\n"):
        return self.stdout.write(msg + eol)

    def _read_history(self):
        try:
            readline.read_history_file('{!s}/.mtui_history'.format(self.homedir))
        except IOError as e:
            logger.debug("failed to open history file: {!s}".format(e))

    def _add_subcommand(self, cmd):
        if cmd.command in self.commands:
            raise CommandAlreadyBoundError(cmd.command)

        self.commands[cmd.command] = cmd

    def set_cmdqueue(self, queue):
        q = queue[:]
        if not self.interactive:
            q.append("quit")

        self.cmdqueue = CmdQueue(q, self.prompt, self.sys)

    def cmdloop(self):
        """
        Customized cmd.Cmd.cmdloop so it handles Ctrl-C and prerun
        """
        while True:
            try:
                cmd.Cmd.cmdloop(self)
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

    def get_names(self):
        names = cmd.Cmd.get_names(self)
        names += ["do_" + x for x in self.commands.keys()]
        names += ["help_" + x for x in self.commands.keys()]
        return names

    def __getattr__(self, x):
        if x.startswith("help_"):
            y = x.replace("help_", "", 1)
            if y in self.commands:
                c = self.commands[y]

                def help():
                    c.argparser(self.sys).print_help()

                return help

        if x.startswith("do_"):
            y = x.replace("do_", "", 1)
            if y in self.commands:
                c = self.commands[y]

                def do(arg):
                    try:
                        args = c.parse_args(arg, self.sys)
                    except ArgsParseFailure:
                        return
                    c(args, self.targets.select(), self.config, self.sys, self).run()

                return do

        if x.startswith("complete_"):
            y = x.replace("complete_", "", 1)
            if y in self.commands:
                c = self.commands[y]

                def complete(*args, **kw):
                    try:
                        if self.metadata and "testopia" in x:
                            try:
                                self.ensure_testopia_loaded()
                            except Exception:
                                logger.debug(format_exc())
                        return c.complete(
                            {
                                "hosts": self.targets.select(),
                                "metadata": self.metadata,
                                "config": self.config,
                                "testopia": self.testopia,
                            },
                            *args,
                            **kw
                        )
                    except Exception as e:
                        logger.error(e)
                        logger.debug(format_exc())
                        raise e

                return complete

        raise AttributeError(str(x))

    def emptyline(self):
        pass

    def ensure_testopia_loaded(self, *packages):
        self.testopia = self.metadata.load_testopia(*packages)

    def set_prompt(self, session=None):
        self.session = session
        session = ":" + str(session) if session else ""
        self.prompt = "mtui{0}> ".format(session)

    def load_update(self, update, autoconnect):
        tr = update.make_testreport(self.config, autoconnect=autoconnect)

        if self.metadata and self.metadata.id is self.session:
            self.set_prompt(None)
        self.metadata = tr
        self.targets = tr.targets

    def _do_save_impl(self, path="log.xml"):
        if not path.startswith("/"):
            dir_ = self.metadata.report_wd()
            path = os.path.join(dir_, 'output', path)

        ensure_dir_exists(os.path.dirname(path))

        if os.path.exists(path):
            logger.warning('file {0} exists.'.format(path))
            m = 'should i overwrite {0}? (y/N) '.format(path)
            if not prompt_user(m, ['y', 'yes'], self.interactive):
                path += '.' + timestamp()

        logger.info("saving output to {0}".format(path))

        with open(path, "w") as f:
            f.write(self.metadata.generate_xmllog())
