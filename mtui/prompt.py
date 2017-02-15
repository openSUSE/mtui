# -*- coding: utf-8 -*-
#
# mtui command line prompt
#

import os
import cmd
import readline
import subprocess

import mtui.notification as notification

from traceback import format_exc

from .argparse import ArgsParseFailure

from mtui import commands
from mtui import messages
from mtui.template import NullTestReport
from mtui.refhost import RefhostsFactory
from mtui.utils import ensure_dir_exists
from mtui.utils import timestamp
from mtui.utils import prompt_user

try:
    unicode
except NameError:
    unicode = str


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
        self.metadata = NullTestReport(config, log)
        self.targets = self.metadata.targets
        """
        alias to ease refactoring
        """

        self.homedir = os.path.expanduser('~')
        self.config = config
        self.log = log
        self.datadir = self.config.datadir

        self.testopia = None

        readline.set_completer_delims('`!@#$%^&*()=+[{]}\|;:",<>? ')

        self._read_history()

        self.commands = {}
        self._add_subcommand(commands.HostsUnlock)
        self._add_subcommand(commands.HostLock)
        self._add_subcommand(commands.Whoami)
        self._add_subcommand(commands.Config)
        self._add_subcommand(commands.ListPackages)
        self._add_subcommand(commands.ReportBug)
        self._add_subcommand(commands.Commit)
        self._add_subcommand(commands.ListBugs)
        self._add_subcommand(commands.ListHosts)
        self._add_subcommand(commands.ListLocks)
        self._add_subcommand(commands.SessionName)
        self._add_subcommand(commands.SetLocation)
        self._add_subcommand(commands.SetLogLevel)
        self._add_subcommand(commands.SetTimeout)
        self._add_subcommand(commands.ListTimeout)
        self._add_subcommand(commands.ListUpdateCommands)
        self._add_subcommand(commands.SetRepo)
        self._add_subcommand(commands.Update)
        self._add_subcommand(commands.RemoveHost)
        self._add_subcommand(commands.ListSessions)
        self._add_subcommand(commands.ListMetadata)
        self._add_subcommand(commands.Downgrade)
        self._add_subcommand(commands.AddHost)
        self._add_subcommand(commands.Install)
        self._add_subcommand(commands.Uninstall)
        self._add_subcommand(commands.Shell)
        self._add_subcommand(commands.Run)
        self._add_subcommand(commands.Prepare)
        self._add_subcommand(commands.OSCAssign)
        self._add_subcommand(commands.OSCApprove)
        self._add_subcommand(commands.OSCReject)
        self._add_subcommand(commands.TestSuiteList)
        self._add_subcommand(commands.TestSuiteRun)
        self._add_subcommand(commands.TestSuiteSubmit)
        self._add_subcommand(commands.ListLog)
        self._add_subcommand(commands.Terms)
        self._add_subcommand(commands.Quit)
        self._add_subcommand(commands.DEOF)
        self._add_subcommand(commands.QExit)
        self._add_subcommand(commands.ListVersions)
        self._add_subcommand(commands.ListHistory)
        self._add_subcommand(commands.DoSave)
        self._add_subcommand(commands.LoadTemplate)
        self._add_subcommand(commands.HostState)
        self._add_subcommand(commands.Export)
        self._add_subcommand(commands.SFTPPut)
        self._add_subcommand(commands.SFTPGet)
        self._add_subcommand(commands.Checkout)
        self._add_subcommand(commands.Edit)
        self._add_subcommand(commands.TestopiaList)
        self._add_subcommand(commands.TestopiaShow)
        self._add_subcommand(commands.TestopiaCreate)
        self._add_subcommand(commands.TestopiaEdit)
        self._add_subcommand(commands.LocalRun)

        self.stdout = self.sys.stdout
        # self.stdout is used by cmd.Cmd
        self.identchars += '-'
        # support commands with dashes in them

    def notify_user(self, msg, class_=None):
        notification.display(self.log, 'MTUI', msg, class_)

    def println(self, msg='', eol='\n'):
        return self.stdout.write(msg + eol)

    def _read_history(self):
        try:
            readline.read_history_file('{!s}/.mtui_history'.format(self.homedir))
        except IOError as e:
            self.log.debug('failed to open history file: {!s}'.format(e))

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
                self.log.error(e)
                self.log.debug(format_exc())
            except Exception as e:
                self.log.error(format_exc())

    def get_names(self):
        names = cmd.Cmd.get_names(self)
        names += ["do_" + x for x in self.commands.keys()]
        names += ["help_" + x for x in self.commands.keys()]
        return names

    def __getattr__(self, x):
        if x.startswith('help_'):
            y = x.replace('help_', '', 1)
            if y in self.commands:
                c = self.commands[y]

                def help():
                    c.argparser(self.sys).print_help()
                return help

        if x.startswith('do_'):
            y = x.replace('do_', '', 1)
            if y in self.commands:
                c = self.commands[y]

                def do(arg):
                    try:
                        args = c.parse_args(arg, self.sys)
                    except ArgsParseFailure:
                        return
                    c(
                        args, self.targets.select(),
                        self.config, self.sys, self.log, self
                    ).run()
                return do

        if x.startswith('complete_'):
            y = x.replace("complete_", "", 1)
            if y in self.commands:
                c = self.commands[y]

                def complete(*args, **kw):
                    try:
                        if self.metadata and 'testopia' in x:
                            try:
                                self.ensure_testopia_loaded()
                            except Exception as e:
                                self.log.debug(format_exc(e))
                        return c.complete({
                            'hosts': self.targets.select(),
                            'metadata': self.metadata,
                            'config': self.config,
                            'log': self.log,
                            'testopia': self.testopia,
                            },
                            *args,
                            **kw)
                    except Exception as e:
                        self.log.error(e)
                        self.log.debug(format_exc(e))
                        raise e
                return complete

        raise AttributeError(str(x))

    def emptyline(self):
        return

    def _refhosts(self):
        try:
            return RefhostsFactory(self.config, self.log)
        except Exception:
            self.log.error('failed to load reference hosts data')
            raise

    def ensure_testopia_loaded(self, *packages):
        self.testopia = self.metadata.load_testopia(*packages)

    def set_prompt(self, session=None):
        self.session = session
        session = ":"+str(session) if session else ''
        self.prompt = 'mtui{0}> '.format(session)

    def load_update(self, update, autoconnect):
        tr = update.make_testreport(
            self.config,
            self.log,
            autoconnect=autoconnect)

        if self.metadata and self.metadata.id is self.session:
            self.set_prompt(None)
        self.metadata = tr
        self.targets = tr.targets

    def _do_save_impl(self, path='log.xml'):
        if not path.startswith('/'):
            dir_ = self.metadata.report_wd()
            path = os.path.join(dir_, 'output', path)

        ensure_dir_exists(os.path.dirname(path))

        if os.path.exists(path):
            self.log.warning('file {0} exists.'.format(path))
            m = 'should i overwrite {0}? (y/N) '.format(path)
            if not prompt_user(m, ['y', 'yes'], self.interactive):
                path += '.' + timestamp()

        self.log.info('saving output to {0}'.format(path))

        with open(path, 'w') as f:
            f.write(self.metadata.generate_xmllog())
