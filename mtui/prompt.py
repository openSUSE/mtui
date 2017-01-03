# -*- coding: utf-8 -*-
#
# mtui command line prompt
#

import os
import cmd
import readline
import subprocess
import glob

from traceback import format_exc

from mtui import messages
from mtui.target import *
from mtui.utils import *
from mtui.refhost import *
import mtui.notification as notification
from mtui import commands
from .argparse import ArgsParseFailure
from mtui.refhost import Attributes
from mtui.template import NullTestReport
from mtui.template import OBSUpdateID
from mtui.utils import requires_update

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
            readline.read_history_file('%s/.mtui_history' % self.homedir)
        except IOError as e:
            self.log.debug('failed to open history file: %s' % str(e))

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
                        return c.complete({
                            'hosts': self.targets.select(),
                            'metadata': self.metadata,
                            'config': self.config,
                            'log': self.log},
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

    def _parse_args(self, cmdline, params_type):
        tavailable = set(self.targets.keys()) | set(['all'])
        tselected = set()
        params = None

        while True:
            arg, _, rest = cmdline.strip().partition(',')
            if arg.strip() in tavailable:
                tselected.add(arg.strip())
                cmdline = rest
            else:
                break

        if params_type == str:
            params = cmdline.strip()
        elif params_type == set:
            params = set([arg.strip()
                         for arg in cmdline.split(',') if arg.strip()])

        if 'all' in tselected or tselected == set():
            targets = self.targets.select(enabled=True)
        else:
            targets = self.targets.select(tselected, enabled=True)

        return (targets, params)

    def do_search_hosts(self, args):
        """
        Seach hosts by by the specified attributes. A attribute tag could also be a
        system type name like sles11sp1-i386 or a hostname.

        search_hosts <attribute> [attribute ...]
        Keyword arguments:
        attribute-- host attributes like architecture or product
        """

        # this is copied in /refsearch.py
        # there is also improved version in
        # qa-maintenance/various-tools.git/refhosts-search

        if not args:
            self.parse_error(self.do_search_hosts, args)
            return

        refhost = self._refhosts()

        if 'Testplatform:' in args:
            # USECASE: this branch is handling a case where user loads mtui
            # without a testreport and copies the Testplatform: line
            # from some testreport into search_hosts or autoadd or
            # loading other set of hosts for running the current update
            # on
            try:
                hosts = refhost.search(Attributes.from_testplatform(
                    args.replace('Testplatform: ', ''), self.log
                ))
            except (ValueError, KeyError):
                self.log.error('failed to parse Testplatform string')
                return []
        elif refhost.get_host_attributes(args):
            hosts = [args]
        else:
            attributes = Attributes.from_search_hosts_query(args)
            hosts = refhost.search(attributes)

            # check if some tags were passed to the attributes object which has
            # all archs set by default
            if not set(str(attributes).split()) ^ set(attributes.tags["archs"]):
                return []

        for hostname in set(hosts):
            hosttags = refhost.get_host_attributes(hostname)
            self.display.search_hosts(hostname, hosttags)

        return hosts

    def complete_search_hosts(self, text, line, begidx, endidx):
        attributes = Attributes()
        return [item for sublist in attributes.tags.values(
            ) for item in sublist if item.startswith(text) and item not in line]

    def do_autoadd(self, args):
        """
        Adds hosts to the target host list. The host is mapped by the
        specified attributes. A attribute tag could also be a system type name
        like sles11sp1-i386 or a hostname.

        autoadd <attribute> [attribute ...]
        attribute-- host attributes like architecture or product
        """

        if not args:
            self.parse_error(self.do_autoadd, args)
            return

        refhost = self._refhosts()
        hosts = self.do_search_hosts(args)

        for hostname in hosts:
            self.metadata.add_target(
                hostname,
                refhost.get_host_systemname(hostname)
            )

    def complete_autoadd(self, text, line, begidx, endidx):
        attributes = Attributes()
        return [item for sublist in attributes.tags.values(
            ) for item in sublist if item.startswith(text) and item not in line]

    def do_list_history(self, args):
        """
        Lists a history of mtui events on the target hosts like installing
        or updating packages. Date, username and event is shown.
        Events could be filtered with the event parameter.

        list_history <hostname>[,...][,event]
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        event    -- connect, disconnect, install, update, downgrade
        None

        """

        if args:
            targets, params = self._parse_args(args, set)

            filters = [
                'connect',
                'disconnect',
                'install',
                'update',
                'downgrade']

            option = [('-e ":%s"' % x) for x in set(params) & set(filters)]

            count = 50
            if len(targets) == len(self.targets):
                count = 10

            targets.report_history(self.display.list_history, count, option)
        else:
            self.parse_error(self.do_list_history, args)

    def complete_list_history(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(
            text, line, begidx, endidx, [
                'connect', 'disconnect', 'install', 'update', 'downgrade'])

    def ensure_testopia_loaded(self, *packages):
        self.testopia = self.metadata.load_testopia(*packages)

    @requires_update
    def do_testopia_list(self, args):
        """
        List all Testopia package testcases for the current product.
        If now packages are set, testcases are displayed for the
        current update.

        testopia_list [package,package,...]
        Keyword arguments:
        package  -- packag to display testcases for
        """

        self.ensure_testopia_loaded(*filter(None, args.split(',')))

        url = self.config.bugzilla_url

        if not self.testopia.testcases:
            self.log.info('no testcases found')

        for tcid, tc in self.testopia.testcases.items():
            self.display.testopia_list(
                url,
                tcid,
                tc['summary'],
                tc['status'],
                tc['automated'])

    @requires_update
    def do_testopia_show(self, args):
        """
        Show Testopia testcase

        testopia_show <testcase>[,testcase,...,testcase]
        Keyword arguments:
        testcase -- testcase ID
        """

        if args:
            cases = []
            url = self.config.bugzilla_url

            self.ensure_testopia_loaded()

            for case in args.split(','):
                case = case.replace('_', ' ')
                try:
                    cases.append(str(int(case)))
                except ValueError:
                    cases = [
                        k for k,
                        v in self.testopia.testcases.items() if v['summary'].replace(
                            '_',
                            ' ') in case]

            for case_id in cases:
                testcase = self.testopia.get_testcase(case_id)

                if not testcase:
                    continue

                if testcase:
                    self.display.testopia_show(
                        url, case_id,
                        testcase['summary'],
                        testcase['status'],
                        testcase['automated'],
                        testcase['requirement'],
                        testcase['setup'],
                        testcase['action'],
                        testcase['breakdown'],
                        testcase['effect'],
                    )
        else:
            self.parse_error(self.do_testopia_show, args)

    def complete_testopia_show(self, text, line, begidx, endidx):
        if not line.count(','):
            return self.complete_testopia_testcaselist(
                text,
                line,
                begidx,
                endidx)

    @requires_update
    def do_testopia_create(self, args):
        """
        Create new Testopia package testcase.
        An editor is spawned to process a testcase template file.

        testopia_create <package>,<summary>
        Keyword arguments:
        package  -- package to create testcase for
        summary  -- testcase summary
        """

        if args:
            url = self.config.bugzilla_url
            testcase = {}
            fields = [
                'requirement:',
                'setup:',
                'breakdown:',
                'action:',
                'effect:']
            (package, _, summary) = args.partition(',')

            self.ensure_testopia_loaded()

            fields.insert(0, 'status: proposed')
            fields.insert(0, 'automated: no')
            fields.insert(0, 'package: %s' % package)
            fields.insert(0, 'summary: %s' % summary)

            try:
                edited = edit_text('\n'.join(fields))
            except subprocess.CalledProcessError as e:
                self.log.error("editor failed: %s" % e)
                self.log.debug(format_exc())
                return

            if edited == '\n'.join(fields):
                self.log.warning('testcase was not modified. not uploading.')
                return

            template = edited.replace('\n', '|br|')

            for field in fields:
                template = template.replace(
                    '|br|%s:' %
                    field.partition(':')[0],
                    '\n%s:' %
                    field.partition(':')[0])

            lines = template.split('\n')
            for line in lines:
                key, _, value = line.partition(':')
                if key == 'package':
                    key = 'tags'
                    value = 'packagename_{name},testcase_{name}'.format(
                        name=value.strip())

                testcase[key] = value.strip()

            try:
                case_id = self.testopia.create_testcase(testcase)
            except Exception:
                self.log.error('failed to create testcase')
            else:
                self.log.info(
                    'created testcase %s/tr_show_case.cgi?case_id=%s' %
                    (url, case_id))

        else:
            self.parse_error(self.do_testopia_create, args)

    def complete_testopia_create(self, text, line, begidx, endidx):
        if not line.count(','):
            return self.complete_packagelist(text, line, begidx, endidx)

    @requires_update
    def do_testopia_edit(self, args):
        """
        Edit already existing Testopia package testcase.
        An editor is spawned to process a testcase template file.

        testopia_edit <testcase>
        Keyword arguments:
        testcase -- testcase ID
        """

        if args:
            template = []
            url = self.config.bugzilla_url
            fields = [
                'summary',
                'automated',
                'status',
                'requirement',
                'setup',
                'breakdown',
                'action',
                'effect']

            self.ensure_testopia_loaded()

            case = args.replace('_', ' ')
            try:
                case_id = str(int(case))
            except ValueError:
                try:
                    case_id = [
                        k for k,
                        v in self.testopia.testcases.items() if v['summary'].replace(
                            '_',
                            ' ') in case][0]
                except IndexError:
                    self.log.critical(
                        'case_id for testcase %s not found' %
                        case)
                    return

            testcase = self.testopia.get_testcase(case_id)

            if not testcase:
                return

            for field in fields:
                template.append('%s: %s' % (field, testcase[field]))

            try:
                edited = edit_text('\n'.join(template))
            except subprocess.CalledProcessError as e:
                self.log.error("editor failed: %s" % e)
                self.log.debug(format_exc())
                return

            if edited == '\n'.join(template):
                self.log.warning('testcase was not modified. not uploading.')
                return

            template = edited.replace('\n', '|br|')

            for field in fields:
                template = template.replace('|br|%s' % field, '\n%s' % field)

            lines = template.split('\n')
            for line in lines:
                key, _, value = line.partition(':')
                testcase[key] = value.strip()

            try:
                self.testopia.modify_testcase(case_id, testcase)
            except Exception:
                self.log.error('failed to modify testcase %s' % case_id)
            else:
                self.log.info(
                    'testcase saved: %s/tr_show_case.cgi?case_id=%s' %
                    (url, case_id))
        else:
            self.parse_error(self.do_testopia_edit, args)

    def complete_testopia_edit(self, text, line, begidx, endidx):
        if not line.count(','):
            return self.complete_testopia_testcaselist(
                text,
                line,
                begidx,
                endidx)

    @requires_update
    def do_list_versions(self, args):
        """
        Prints the package version history in chronological order.
        The history of every test host is checked and consolidated.
        If no packages are specified, the version history of the
        update packages are shown.

        list_versions [package,...,package]
        Keyword arguments:
        package  -- packagename to show version history
        """

        """
        example output:

        mtui> list_versions
        version history from:
          s390vsl048.suse.de (sles12None-s390x)

        libzmq3:
        -> 4.0.4-2.1

        zeromq-devel:
        -> 4.0.4-2.1

        version history from:
          edna.qam.suse.de (sles12None-x86_64)
          bart.qam.suse.de (sled12None-x86_64)
          moe.qam.suse.de (sles12None-x86_64)

        libzmq3:
        -> 4.0.4-4.1
          -> 4.0.4-2.1

        zeromq-devel:
        -> 4.0.4-4.1
          -> 4.0.4-2.1

        --
        FIXME: output of this command includes the wording "version history",
          while it lists versions available from the host's repositories
          (uses `zypper search`).

        """

        targets, params = self._parse_args(args, set)

        if not targets:
            return

        self.metadata.list_versions(
            self.display.list_versions,
            targets,
            params)

    def set_prompt(self, session=None):
        self.session = session
        session = ":"+str(session) if session else ''
        self.prompt = 'mtui{0}> '.format(session)

    def do_load_template(self, args):
        """
        Load QA Maintenance template by RRID identifier. All changes and logs
        from an already loaded template are lost if not saved previously.
        Already connected hosts are kept and extended by the reference hosts
        defined in the template file.

        load_template <update_id>
        Keyword arguments:
        update_id      -- obs request review id for obs update """

        id_ = args.strip()
        try:
            update = OBSUpdateID(id_)
        except ValueError:
            pass

        if not update:
            raise ValueError("Couldn't match {0!r} to either of {1!r}".
                             format(id_, u_types))

        if self.metadata:
            m = 'should i overwrite already loaded session {0}? (y/N) '
            if not prompt_user(m.format(self.metadata.id), ['y', 'yes'], self.interactive):
                return

        # Reload hosts to which we already have a connection
        # close hosts we are already connected to but add them to the
        # testreport.systems so they get connected to again.
        # This feature comes from pre-1.0 versions.
        # NOTE: the only reason we need to reconnect seems to be that
        # when the L{Target} object is created, it is passed a list of
        # packages, which changes with the testreport change. So this
        # may go away when refactored.
        re_add = []
        for hostname, target in self.targets.items():
            target.close()
            re_add.append((hostname, target.system))

        self.load_update(update, autoconnect=True)

        for hostname, system in re_add:
            self.metadata.add_target(hostname, system)

    def load_update(self, update, autoconnect):
        tr = update.make_testreport(
            self.config,
            self.log,
            autoconnect=autoconnect)

        if self.metadata and self.metadata.id is self.session:
            self.set_prompt(None)
        self.metadata = tr
        self.targets = tr.targets

    def do_set_host_lock(self, args):
        """
        Lock host for exclusive usage. This locks all repository transactions
        like enabling or disabling the testing repository on the target hosts.
        The Hosts are locked with a timestamp, the UID and PID of the session.
        This influences the update process of concurrent instances, use with
        care.
        Enabled locks are automatically removed when exiting the session.
        To lock the run command on other sessions as well, it's necessary to
        set a comment.

        set_host_lock <hostname>[,hostname,...],<state>
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        state    -- enabled, disabled
        """

        targets, state = self._parse_args(args, str)

        if targets and state:

            if state == 'enabled':
                comment = user_input('comment: ').strip()

            for target in targets:
                lock = targets[target].locked()

                if state == 'enabled':
                    if lock.locked:
                        self.log.warning(
                            'host %s is locked since %s by %s. skipping.' %
                            (target, lock.time(), lock.user))
                        if lock.comment:
                            self.log.info(
                                "%s's comment: %s" %
                                (lock.user, lock.comment))

                        continue
                    else:
                        targets[target].set_locked(comment)
                elif state == 'disabled':
                    msg = "set_host_lock <host>,disable has been"
                    msg += " deprecated in favor of unlock command"
                    user_deprecation(self.log, msg)

                    try:
                        targets[target].remove_lock()
                    except AssertionError:
                        self.log.warning(
                            'host %s not locked by us. skipping.' %
                            target)
                else:
                    self.parse_error(self.do_set_host_lock, args)
        else:

            self.parse_error(self.do_set_host_lock, args)
            return

    def complete_set_host_lock(self, text, line, begidx, endidx):
        if line.count(','):
            return self.complete_enabled_hostlist(
                text, line, begidx, endidx, [
                    'enabled', 'disabled'])
        else:
            return self.complete_enabled_hostlist_with_all(
                text,
                line,
                begidx,
                endidx)

    def do_set_host_state(self, args):
        """
        Sets the host state to "Enabled", "Disabled" or "Dryrun". A host
        set to "Enabled" runs all issued commands while a "Disabled" host
        or a host set to "Dryrun" doesn't run any command on the host.
        The difference between "Disabled" and "Dryrun" is that on "Dryrun"
        hosts the issued commands are printed to the console while "Disabled"
        doesn't print anything. Additionally, the execution mode of each host
        could be set to "parallel" (default) or "serial". All commands which
        are designed to run in parallel are influenced by this option (like
        to run command)
        The commands accepts multiple hostnames followed by the wanted state.

        set_host_state <hostname>[,hostname,...],<state>
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        state    -- enabled, disabled, dryrun, parallel, serial
        """

        targets, state = self._parse_args(args, str)

        if targets and state:

            if state in ['enabled', 'disabled', 'dryrun']:
                for target in targets:
                    targets[target].state = state
            elif state in ['parallel', 'serial']:
                for target in targets:
                    if state == 'serial':
                        targets[target].exclusive = True
                    else:
                        targets[target].exclusive = False
            else:
                self.parse_error(self.do_set_host_state, args)
                return
        else:

            self.parse_error(self.do_set_host_state, args)

    def complete_set_host_state(self, text, line, begidx, endidx):
        if line.count(','):
            return self.complete_hostlist(
                text, line, begidx, endidx, [
                    'enabled', 'disabled', 'dryrun', 'serial', 'parallel'])
        else:
            return self.complete_hostlist_with_all(text, line, begidx, endidx)

    @requires_update
    def do_checkout(self, args):
        """
        Update template files from the SVN.

        checkout
        Keyword arguments:
        none
        """

        try:
            subprocess.check_call(
                'svn up'.split(),
                cwd=self.metadata.report_wd())
        except Exception:
            self.log.error('updating template failed')
            self.log.debug(format_exc())

    def do_put(self, args):
        """
        Uploads files to all enabled hosts. Multiple files can be selected
        with special patterns according to the rules used by the Unix shell
        (i.e. *, ?, []). The complete filepath on the remote hosts is shown
        after the upload. put has also directory completion.

        put <local filename>
        Keyword arguments:
        filename -- file to upload to the target hosts
        """

        if not args:
            self.parse_error(self.do_put, args)
            return

        for filename in glob.glob(args):
            if not os.path.isfile(filename):
                continue

            remote = self.metadata.target_wd(os.path.basename(filename))

            self.targets.put(filename, remote)
            self.log.info('uploaded {0} to {1}'.format(filename, remote))

    def complete_put(self, text, line, begidx, endidx):
        return self.complete_filelist(text, line, begidx, endidx)

    def do_get(self, args):
        """
        Downloads a file from all enabled hosts. Multiple files cannot be
        selected. Files are saved in the $TEMPLATE_DIR/downloads/ subdirectory
        with the hostname as file extension. If the argument ends with a
        slash '/', it will be treated as a folder and all its contents will
        be downloaded.

        get <remote filename>
        Keyword arguments:
        filename -- file to download from the target hosts
        """

        if not args:
            self.parse_error(self.do_get, args)
            return

        self.metadata.perform_get(self.targets, args)

        self.log.info('downloaded {0}'.format(args))

    def do_terms(self, args):
        """
        Spawn terminal screens to all connected hosts. This command does
        actually just run the available helper scripts. If no termname is
        given, all available terminal scripts are shown.

        script name should be shell.<termname>.sh

        terms [termname]
        Keyword arguments:
        termname -- terminal emulator to spawn consoles on
        """

        systems = {}
        dirname = self.datadir
        targets = self.targets

        hosts = [host.hostname for host in sorted(targets.values())]

        if args:
            filename = 'term.' + args + '.sh'
            path = os.path.join(dirname, filename)
            if os.path.isfile(path):
                try:
                    subprocess.check_call([path] + hosts)
                except Exception:
                    self.log.error('running %s failed' % filename)
                    self.log.debug(format_exc())
            else:
                self.log.error(
                    '%s script not found, make sure term.%s.sh exists' %
                    (args, args))
                self.parse_error(self.do_terms, args)
        else:

            self.println('available terminals scripts:')
            for filename in glob.glob(os.path.join(dirname, 'term.*.sh')):
                self.println(os.path.basename(filename).split('.')[1])

    def complete_terms(self, text, line, begidx, endidx):
        dirname = self.datadir
        terms = glob.glob(os.path.join(dirname, 'term.*.sh'))
        terms = map(os.path.basename, terms)
        return [i.split('.')[1] for i in terms if i.startswith('term.' + text)]

    def do_edit(self, args):
        """
        Edit a local file, the testing template, the specfile or a patch.
        The evironment variable EDITOR is processed to find the prefered
        editor. If EDITOR is empty, "vi" is set as default.

        edit file,<filename>
        edit template
        Keyword arguments:
        filename -- edit filename
        template -- edit template
        """

        (command, _, filename) = args.partition(',')

        editor = os.environ.get('EDITOR', 'vi')

        # all but the file command needs template data. skip if template
        # isn't loaded
        if not self.metadata and command != 'file':
            self.log.error('no testing template loaded')
            return

        if command == 'file':
            path = filename
        elif command == 'template':
            path = self.metadata.path
        else:
            self.parse_error(self.do_edit, args)
            return

        try:
            subprocess.check_call([editor, path])
        except Exception:
            self.log.error("failed to run %s" % editor)
            self.log.debug(format_exc())

    def complete_edit(self, text, line, begidx, endidx):
        if 'file,' in line:
            return self.complete_filelist(
                text.replace(
                    'file,',
                    '',
                    1),
                line,
                begidx,
                endidx)
        else:
            return [
                i for i in [
                    'file,',
                    'template'] if i.startswith(text)]

    @requires_update
    def do_export(self, args):
        """
        Exports the gathered update data to template file. This includes
        the pre/post package versions and the update log. An output file could
        be specified, if none is specified, the output is written to the
        current testing template.
        To export a specific updatelog, provide the hostname as parameter.

        export [filename][,hostname][,force]
        Keyword arguments:
        filename -- output template file name
        hostname -- host update log to export
        force    -- overwrite template if it exists
        """

        force = False
        hostname = None
        filename = self.metadata.path

        parameters = filter(None, args.split(','))
        for parameter in list(parameters):
            if parameter in ['force']:
                force = True
                parameters.remove('force')
            if parameter in self.targets:
                hostname = parameter
                parameters.remove(hostname)

        if parameters:
            filename = parameters[0]

        try:
            template = self.metadata.generate_templatefile(hostname)
        except Exception as e:
            self.log.error('failed to export XML')
            self.log.error(e)
            self.log.debug(format_exc(e))
            return

        if os.path.exists(filename) and not force:
            self.log.warning('file %s exists.' % filename)
            if not prompt_user('should i overwrite %s? (y/N) ' % filename, ['y', 'yes'], self.interactive):
                filename += '.' + timestamp()

        self.log.info('exporting XML to %s' % filename)
        try:
            with open(filename, 'w') as f:
                f.write('\n'.join(l.rstrip().encode('utf-8')
                        for l in template))
        except IOError as error:
            self.println(
                'failed to write {}: {}'.format(
                    filename,
                    error.strerror))
        else:
            self.println('wrote template to {}'.format(filename))

    def complete_export(self, text, line, begidx, endidx):
        return self.complete_hostlist(text, line, begidx, endidx, ['force'])

    def do_save(self, args):
        """
        Save the testing log to a XML file. All commands and package
        versions are saved there. When no parameter is given, the XML is saved
        to $TEMPLATE_DIR/output/log.xml. If that file already exists and the
        tester doesn't want to overwrite it, a postfix (current timestamp)
        is added to the filename. The log can be used to fill the required
        sections of the testing template after the testing has finished.
        This could be done with the convert.py script.

        save [filename]
        Keyword arguments:
        filename -- save log as file filename
        """

        path = [args.strip()] if args else []
        self._do_save_impl(*path)

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

    def do_quit(self, args):
        """
        Disconnects from all hosts and exits the programm. If a bootarg
        argument is set, the hosts are either rebooted or powered off.
        The tester is asked to save the XML log when exiting MTUI.

        quit [bootarg]
        Keyword arguments:
        bootarg  -- reboot or poweroff
        """

        if not prompt_user('save log? (Y/n) ', ['n', 'no'], self.interactive):
            self._do_save_impl()

        args_ = [args] if args in ('reboot', 'poweroff') else []

        for x in set(self.targets):
            self.targets[x].close(*args_)
            self.targets.pop(x)

        try:
            readline.write_history_file('%s/.mtui_history' % self.homedir)
        except:
            pass

        self.sys.exit(0)

    def complete_quit(self, text, line, begidx, endidx, appendix=[]):
        return [i for i in ["reboot", "poweroff"] if i.startswith(text)]

    do_exit = do_quit
    do_EOF = do_quit

    def complete_filelist(self, text, line, begidx, endidx):
        dirname = ''
        filename = ''

        if text.startswith('~'):
            text = text.replace('~', os.path.expanduser('~'), 1)
            text += '/'

        if '/' in text:
            dirname = '/'.join(text.split('/')[:-1])
            dirname += '/'

        if not dirname:
            dirname = './'

        filename = text.split('/')[-1]

        return [
            dirname +
            i for i in os.listdir(dirname) if i.startswith(filename)]

    def complete_hostlist(self, text, line, begidx, endidx, appendix=[]):
        return [
            i for i in list(
                self.targets) +
            appendix if i.startswith(text) and i not in line]

    def complete_hostlist_with_all(
            self,
            text,
            line,
            begidx,
            endidx,
            appendix=[]):
        return [
            i for i in list(
                self.targets) +
            ['all'] +
            appendix if i.startswith(text) and i not in line]

    def complete_enabled_hostlist(
            self,
            text,
            line,
            begidx,
            endidx,
            appendix=[]):
        return [
            i for i in list(
                self.targets.select(
                    enabled=True)) +
            appendix if i.startswith(text) and i not in line]

    def complete_enabled_hostlist_with_all(
            self,
            text,
            line,
            begidx,
            endidx,
            appendix=[]):
        return [
            i for i in list(
                self.targets.select(
                    enabled=True)) +
            ['all'] +
            appendix if i.startswith(text) and i not in line]

    def complete_packagelist(self, text, line, begidx, endidx, appendix=[]):
        return [i for i in self.metadata.get_package_list() if i.startswith(
            text) and i not in line]

    def complete_testopia_testcaselist(self, text, line, begidx, endidx):
        self.ensure_testopia_loaded()

        testcases = [
            i['summary'].replace(
                ' ',
                '_') for i in self.testopia.testcases.values()]
        return [i for i in testcases if i.startswith(text) and i not in line]

    def parse_error(self, method, args):
        self.println()
        self.log.error(
            'failed to parse command: %s %s' %
            (method.__name__.replace(
                'do_',
                ''),
                args))
        self.println(
            '{}: {}'.format(
                method.__name__.replace(
                    'do_',
                    ''),
                method.__doc__))


def user_deprecation(log, msg):
    log.warning(msg)
