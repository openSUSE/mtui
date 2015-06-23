# -*- coding: utf-8 -*-
#
# mtui command line prompt
#

from functools import reduce

import itertools
import os
import sys
import stat
import errno
import cmd
import logging
import readline
import subprocess
import glob
import re
import getpass
import shutil
from traceback import print_exc
from os.path import join
from os.path import isfile
from os.path import basename
from os.path import splitext

from datetime import datetime

from mtui import messages
from mtui.rpmver import *
from mtui.target import *
from mtui.export import *
from mtui.utils import *
from mtui.refhost import *
from mtui.config import *
from mtui.notification import *
from mtui import commands, strict_version
from mtui.utils import log_exception
from .argparse import ArgsParseFailure
from mtui.refhost import Attributes
from mtui.types import MD5Hash
from mtui.types import obs
from mtui.template import NullTestReport
from mtui.template import OBSUpdateID
from mtui.template import SwampUpdateID
from mtui import updater
from mtui.utils import requires_update
from mtui.utils import ass_is, ass_isL

from distutils.version import StrictVersion

out = logging.getLogger('mtui')

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
    def __init__(self, iterable, prompt, term = None):
        self.prompt = prompt
        self.term = term or sys
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

    def __init__(self, config, log, sys_=None):
        self.set_prompt()
        self.sys = sys_ or sys

        cmd.Cmd.__init__(self, stdout=self.sys.stdout, stdin=self.sys.stdin)
        self.interactive = True

        self.metadata = NullTestReport(config, log)
        self.targets = self.metadata.targets
        """
        alias to ease refactoring
        """

        self.homedir = os.path.expanduser('~')
        self.config = config
        self.log = log
        self.datadir = self.config.datadir

        self.set_interface_version(config.interface_version)

        self.testopia = None

        readline.set_completer_delims('`!@#$%^&*()=+[{]}\|;:",<>? ')

        self._read_history()

        self.commands = {}
        self._add_subcommand(commands.HostsUnlock)
        self._add_subcommand(commands.Whoami)
        self._add_subcommand(commands.Config)
        self._add_subcommand(commands.ListPackages)
        self._add_subcommand(commands.ReportBug)
        self.sys = sys_ or sys
        self.stdout = self.sys.stdout
        # self.stdout is used by cmd.Cmd
        self.identchars += '-'
        # support commands with dashes in them

    def println(self, msg = '', eol = '\n'):
        return self.stdout.write(msg + eol)

    def get_interface_version(self):
        """
        :return: L{StrictVersion} instance
        """
        return self._interface_version

    def set_interface_version(self, x):
        """
        :type x: L{StrictVersion} or convertible to it.
        :param x: version

        :return: None
        """
        if not isinstance(x, StrictVersion):
            x = StrictVersion(x)

        self._interface_version = x

    def _read_history(self):
        try:
            readline.read_history_file('%s/.mtui_history' % self.homedir)
        except IOError as e:
            out.debug('failed to open history file: %s' % str(e))

    def _add_subcommand(self, cmd):
        if cmd.command in self.commands:
            raise CommandAlreadyBoundError(cmd.command)

        if self._interface_version < StrictVersion(cmd.stable):
            return

        self.commands[cmd.command] = cmd

    def set_cmdqueue(self, queue):
        q = queue[:]
        if not self.interactive:
            q.append("quit")

        self.cmdqueue = CmdQueue(q, self.prompt, term = self.sys)

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
            except (messages.UserError, subprocess.CalledProcessError) as e:
                self.log.error(e)
                self.log.debug(format_exc())
            except Exception as e:
                self.log.error(format_exc())

    # {{{ overrides to support new style commands
    def onecmd(self, line):
        # FIXME: see L{CommandPrompt.__getattr__}
        cmd_, arg, line = self.parseline(line)
        try:
            subcmd = self.commands[cmd_]
        except KeyError:
            return cmd.Cmd.onecmd(self, line)

        try:
            args = subcmd.parse_args(arg, self.sys)
        except ArgsParseFailure:
            return

        subcmd(
            args, self.targets.select(enabled = True),
            self.config, self.sys, self.log, self
        ).run()

    def do_help(self, arg):
        # FIXME: see L{CommandPrompt.__getattr__}
        try:
            cmd_ = self.commands[arg]
        except KeyError:
            return cmd.Cmd.do_help(self, arg)
        else:
            cmd_.argparser(self.sys).print_help()

    def get_names(self):
        names = cmd.Cmd.get_names(self)
        names = names + ["do_" + x for x in self.commands.keys()]
        return names

    def __getattr__(self, x):
        # Currently, it appears the purpose is to
        #
        # 1. return newly constructed object with .__doc__ set to
        #    commands argparser generated help if x is "do_<cmd>"
        #
        # 2. return completer generated by the L{Command.completer}
        #
        # However, 1 seems to be moot, since there is
        # L{CommandPrompt.do_help} that handles CDC's extra and this is
        # leftover dead code and not returning callable seems fine since
        # that's also handled extra in L{CommandPrompt.onecmd}
        #
        # so FIXME:
        #
        # 1. fix L{Command} so that __doc__ -> argparser.format_help()
        #    is handled internally there.
        #
        # 2. fix L{Command} so that it provides __call__ and the
        #    Command instance can be returned for "do_<cmd>", therefore
        #    removing the need to override both do_help and onecmd.
        try:
            y = x
            if isinstance(y, str):
                y = y.replace("do_","")
            c = self.commands[y]
        except KeyError:
            try:
                y = x
                if isinstance(y, str):
                    y = y.replace("complete_","")
                c = self.commands[y]
            except KeyError:
                raise AttributeError(str(x))
            else:
                return log_exception(Exception, out.error)\
                    (c.completer(self.targets.select(enabled = True)))
        else:
            argparser = c.argparser(self.sys)
            clsdict = {
                '__doc__': argparser.format_help()
            }
            return type(x, (object,), clsdict)
    # }}}

    def emptyline(self):
        return

    def _refhosts(self):
        try:
            return RefhostsFactory(self.config, self.log)
        except Exception:
            out.error('failed to load reference hosts data')
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
            params = set([arg.strip() for arg in cmdline.split(',') if arg.strip()])

        if 'all' in tselected or tselected == set():
            targets = self.targets.select(enabled = True)
        else:
            targets = self.targets.select(tselected, enabled = True)

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

        refhost = self._refhosts()

        if 'Testplatform:' in args:
            # USECASE: this branch is handling a case where user loads mtui
            # without a testreport and copies the Testplatform: line
            # from some testreport into search_hosts or autoadd or
            # loading other set of hosts for running the current update
            # on
            try:
                hosts = refhost.search(Attributes.from_testplatform(
                  args.replace('Testplatform: ', '')
                , self.log
                ))
            except (ValueError, KeyError):
                out.error('failed to parse Testplatform string')
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
            self.println('{0:25}: {1}'.format(hostname, hosttags))

        return hosts

    def complete_search_hosts(self, text, line, begidx, endidx):
        attributes = Attributes()
        return [item for sublist in attributes.tags.values() for item in sublist if item.startswith(text) and item not in line]

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
            self.connect_system_if_unconnected(
                hostname,
                refhost.get_host_systemname(hostname)
            )

    def complete_autoadd(self, text, line, begidx, endidx):
        attributes = Attributes()
        return [item for sublist in attributes.tags.values() for item in sublist if item.startswith(text) and item not in line]

    def connect_system_if_unconnected(self, hostname, system):
        try:
            out.warning('already connected to {0}. skipping.'.format(
                self.targets[hostname].hostname
            ))
        except KeyError:
            self.targets[hostname] = Target(hostname, system, self.metadata.get_package_list())

            if self.metadata:
                self.metadata.systems[hostname] = system

    def do_add_host(self, args):
        """
        Adds another machine to the target host list. The system type needs
        to be specified as well.

        add_host <hostname,system>
        Keyword arguments:
        hostname -- address of the target host (should be the FQDN)
        system   -- system type, ie. sles11sp1-i386
        """

        if not args:
            self.parse_error(self.do_add_host, args)
            return

        try:
            (hostname, system) = args.split(',')
        except ValueError:
            self.parse_error(self.do_add_host, args)
            return

        self.connect_system_if_unconnected(hostname, system)

    def do_remove_host(self, args):
        """
        Disconnects from host and remove host from list. Warning: The host
        log is purged as well. If the tester wants to preserve the log, it's
        better to use the "set_host_state" command instead and set
        the host to "disabled". Multible hosts can be specified.

        remove_host <hostname>[,hostname,...]
        Keyword arguments:
        hostname -- hostname from the target list
        """

        if not args:
            self.parse_error(self.do_remove_host, args)
            return

        targets, _ = self._parse_args(args, None)
        for tgt in targets:
            self.targets[tgt].close()
            self.targets.pop(tgt)

    def complete_remove_host(self, text, line, begidx, endidx):
        return self.complete_hostlist_with_all(text, line, begidx, endidx)

    def do_list_hosts(self, args):
        """
        Lists all connected hosts including the system types and their
        current state. State could be "Enabled", "Disabled" or "Dryrun".

        list_hosts
        Keyword arguments:
        None
        """

        if args:
            self.parse_error(self.do_list_hosts, args)
        else:

            targets = self.targets

            for host in sorted(targets.values()):
                if host.exclusive:
                    mode = 'serial'
                else:
                    mode = 'parallel'

                if host.state == 'enabled':
                    state = green('Enabled')
                elif host.state == 'dryrun':
                    state = yellow('Dryrun')
                else:
                    state = red('Disabled')

                system = '(%s)' % host.system
                self.println('{0:20} {1:20}: {2} ({3})'.format(
                    host.hostname,
                    system,
                    state,
                    mode
                ))

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

            filters = ['connect', 'disconnect', 'install', 'update', 'downgrade']

            option = [('-e ":%s"' % x) for x in set(params) & set(filters)]

            count = 50
            if len(targets) == len(self.targets):
                count = 10

            if targets:
                if option:
                    RunCommand(targets, 'tac /var/log/mtui.log | grep -m %s %s | tac' % (count, ' '.join(option))).run()
                else:
                    RunCommand(targets, 'tail -n %s /var/log/mtui.log' % count).run()

            for host in sorted(targets.values()):
                self.println('history from {} ({}):'.format(
                    host.hostname,
                    host.system
                ))
                lines = host.lastout().split('\n')
                lines.reverse()
                for line in lines:
                    try:
                        when = line.split(':')[0]
                        who = line.split(':')[1]
                        event = ':'.join(line.split(':')[2:])
                    except IndexError:
                        continue

                    time = datetime.fromtimestamp(float(when))
                    self.println('{}, {}: {}'.format(
                        time.strftime('%A, %d.%m.%Y %H:%M'),
                        who,
                        event
                    ))
                self.println()
        else:
            self.parse_error(self.do_list_history, args)

    def complete_list_history(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx,
                ['connect', 'disconnect', 'install', 'update', 'downgrade'])

    def do_list_locks(self, args):
        """
        Lists lock state of all connected hosts

        list_hosts
        Keyword arguments:
        None
        """

        if args:
            self.parse_error(self.do_list_hosts, args)
        else:

            targets, _ = self._parse_args(args, None)

            for host in sorted(targets.values()):
                system = '(%s)' % host.system
                lock = host.locked()
                if lock.locked:
                    if lock.own():
                        lockedby = 'me'
                    else:
                        lockedby = lock.user

                    self.println(eol = '', msg = '{0:20} {1:20}: {2}'.format(
                        host.hostname,
                        system,
                        yellow('since {} by {}'.format(lock.time(), lockedby))
                    ))
                    if lock.comment:
                        self.println(' : {}'.format(lock.comment))
                    else:
                        self.println()
                else:
                    self.println('{0:20} {1:20}: {2}'.format(
                        host.hostname,
                        system,
                        green('not locked')
                    ))

    def do_list_timeout(self, args):
        """
        Prints the current timeout values per host in seconds.

        list_timeout
        Keyword arguments:
        None
        """

        if args:
            self.parse_error(self.do_list_timeout, args)
        else:

            targets = self.targets

            for host in sorted(targets.values()):
                system = '(%s)' % host.system
                timeout = host.get_timeout()
                self.println('{0:20} {1:20}: {2}s'.format(
                    host.hostname,
                    system,
                    timeout
                ))

    @requires_update
    def do_source_extract(self, _):
        """
        Extracts current source RPMs to a local temporary directory.
        """
        self.metadata.extract_source_rpm()

    @requires_update
    def do_source_diff(self, args):
        """
        Creates a source diff between the updated package (read from
        testreport) and the currently installed package on the SUT.

        If the diff needs to be against the latest released package,
        make sure to run "prepare" first.

        Diff type "source" creates a diff of the specfile and new patchfiles.

        Diff type "build" creates a diff between the patched build
        directories which may be architecture dependent.

        The osc command line client needs to be installed first.

        source_diff <type>
        Keyword arguments:
        type     -- "build" or "source" diff
        """

        targets, mode = self._parse_args(args, str)

        if mode not in ['source', 'build']:
            self.parse_error(self.do_source_diff, args)

        try:
            import osc
            from osc import commandline
        except:
            out.error('missing osc module. please install osc and setup an account.')
            return

        api_config_options = {'https://api.suse.de': {'http_headers': [], 'sslcertck': True, 'user': 'qa', 'pass': 'qa'}}
        osc.conf.config['api_host_options'] = api_config_options
        osc.conf.config['debug'] = 0
        osc.conf.config['verbose'] = 0
        osc.conf.config['http_debug'] = 0

        updated = {}
        installed = {}
        destination = self.metadata.local_wd()

        if not glob.glob(os.path.join(destination, '*', '*.spec')):
            self.metadata.extract_source_rpm()

        for rpmfile in glob.glob(os.path.join(destination, '*.src.rpm')):
            try:
                rpmf = RPMFile(rpmfile)
            except Exception as error:
                out.critical('failed to open %s: %s' % (rpmfile, error))
                if unicode(error) == u'public key not available':
                    out.critical('Public key is not available.')
                    out.critical('In order to import new keys, you should run the following command as root:')
                    out.critical('cd /tmp; wget -q -r -nd -l1 --no-parent -A "*.asc" http://download.suse.de/keys/; for i in *.asc; do rpm --import $i; done')
                continue

            try:
                durl = obs.DistURL(rpmf.disturl)
            except messages.ErrorMessage as e:
                out.warning(e)
            else:
                # NOTE: it's important not to confuse rpmf.name and
                # durl.package. See L{obs.DistURL}
                out.debug("rpmf.name: %s" % rpmf.name)
                out.debug("durl.package: %s" % durl.package)
                updated[rpmf.name] = durl

        # if there are src.rpm package names which are not reflected by
        # binary rpms, check all binary rpms for this specific
        # src.rpm/disturl name
        if [x for x in updated.keys() if x not in self.metadata.get_package_list()]:
            search_list = self.metadata.get_package_list()
        else:
            search_list = list(updated.keys())

        out.debug("search_list: {}".format(search_list))

        for package in search_list:
            RunCommand(targets, 'rpm -q --qf "%%{DISTURL}" %s' % package).run()

            for target in targets.values():
                line = target.lastout().split('\n')[0]

                try:
                    durl = obs.DistURL(line)
                except messages.ErrorMessage as e:
                    out.warning(e)
                else:
                    installed[package] = durl

        out.debug("updated: {}".format(updated.keys()))
        out.debug("installed: {}".format(installed.keys()))

        for name in updated.keys():
            try:
                di = installed[name]
                du = updated[name]
                assert(di and du)
            except (AssertionError, KeyError):
                out.warning('osc disturl not found for package %s. skipping.' % name)
                continue

            if di.commit == du.commit:
                out.warning(messages.PackageRevisionHasntChangedWarning(name))
                continue

            diff = os.path.join(destination, '%s-%s.diff' % (name, mode))
            if mode == 'source':
                with open(diff, 'w+') as f:
                    try:
                        f.write(osc.core.server_diff(
                              'https://api.suse.de'
                            , di.project
                            , di.package
                            , di.commit
                            , du.project
                            , du.package
                            , du.commit
                            , unified=True
                        ))
                    except Exception as error:
                        out.error('failed to diff packages: %s', error)
                        return

                out.info('wrote diff locally to %s' % diff)

            elif mode == 'build':
                RunCommand(targets, 'which osc').run()
                for target in targets:
                    if targets[target].lastexit() != 0:
                        out.error('osc is missing on %s. skipping.' % target)

                for state in ['new', 'old']:
                    sourcedir = os.path.join(destination, name, state)
                    builddir = os.path.join(destination, name, state, 'BUILD')
                    disturl = du.disturl if state == 'new' else di.disturl

                    RunCommand(targets, 'echo "[general]\n[https://api.suse.de]\nuser = qa\npass = qa" >/tmp/osc.mtui').run()
                    RunCommand(targets, 'mkdir -p %s' % builddir).run()
                    RunCommand(targets, 'cd %s; osc -c /tmp/osc.mtui -q -A "https://api.suse.de" co -c %s' % (sourcedir, disturl)).run()
                    RunCommand(targets, 'rpmbuild --quiet --nodeps --define "_sourcedir %s/%s" --define "_builddir %s" -bp %s/%s/*.spec'
                            % (sourcedir, name, builddir, sourcedir, name)).run()

                RunCommand(targets, 'diff -x ".osc" -Naur %s/../old/BUILD %s/../new/BUILD > %s' % (sourcedir, sourcedir, diff)).run()

                out.info('wrote diff remotely to %s' % diff)

    def complete_source_diff(self, text, line, begidx, endidx):
        return [i for i in ['source', 'build'] if i.startswith(text)]

    @requires_update
    def do_source_verify(self, args):
        """
        Verifies SPECFILE content. Makes sure that every Patch entry
        is applied.

        source_verify
        Keyword arguments:
        None
        """

        if args:
            self.parse_error(self.do_source_verify, args)

        destination = self.metadata.local_wd()

        specfiles = glob.glob(os.path.join(destination, '*', '*.spec'))

        if not specfiles:
            self.metadata.extract_source_rpm()
            specfiles = glob.glob(os.path.join(destination, '*', '*.spec'))
            if not specfiles:
                out.error('failed to load specfile')
                return

        out.debug("Found specfiles: {0}".format(specfiles))
        for specfile in specfiles:
            patches = {}
            with open(specfile, 'r') as spec:
                content = spec.readlines()

            for line in content:
                match = re.search('^Name:\W+(.*)', line)
                if match:
                    name = match.group(1)

                match = re.search('^(Patch\d*):\W+(.*)', line)
                if match:
                    patches[match.group(1)] = match.group(2)

            self.println()

            if not patches:
                out.warning('no patch entries found in specfile {0}'
                    .format(specfile))
            else:
                self.println('Patches in {}:'.format(specfile))

                for patch in patches:
                    num = ''.join(filter(str.isdigit, patch)) or 0
                    if num == 0 and re.findall('\'%patch\W+', str(content)):
                        result = green('applied')
                    elif re.findall('\'%%%s%s\W+' % ('patch', num), str(content)):
                        result = green('applied')
                    elif re.findall('patch.*%%{P:%s}' % num, str(content)):
                        result = green('applied')
                    else:
                        result = red('not applied')

                    self.println('{0:45}: {1}'.format(
                        patches[patch].replace('name}', name),
                        result
                    ))

    def complete_list_packages(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    @requires_update
    def do_list_update_commands(self, args):
        """
        List all commands which are invoked when applying updates on the
        target hosts.

        list_update_commands
        Keyword arguments:
        None
        """

        if args:
            self.parse_error(self.do_list_update_commands, args)
        else:
            self.metadata.list_update_commands(self.targets, self.println)

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

        url = config.bugzilla_url

        if not self.testopia.testcases:
            out.info('no testcases found')

        for tcid, tc in self.testopia.testcases.items():
            summary = tc['summary']
            if tc['status'] == 'disabled':
                status = red('disabled')
            elif tc['status'] == 'confirmed':
                status = green('confirmed')
            else:
                status = yellow('proposed')
            if tc['automated'] == 'yes':
                automated = 'automated'
            else:
                automated = 'manual'
            self.println('{0:40}: {1} ({2})'.format(summary, status, automated))
            self.println('{}/tr_show_case.cgi?case_id={}'.format(url, tcid))
            self.println()

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
            url = config.bugzilla_url

            self.ensure_testopia_loaded()

            for case in args.split(','):
                case = case.replace('_', ' ')
                try:
                    cases.append(str(int(case)))
                except ValueError:
                    cases = [ k for k, v in self.testopia.testcases.items() if v['summary'].replace('_', ' ') in case ]

            for case_id in cases:
                testcase = self.testopia.get_testcase(case_id)

                if not testcase:
                    continue

                if testcase:
                    self.println('%s %s' % (blue('Testcase summary:'), testcase['summary']))
                    self.println('%s %s' % (blue('Testcase URL:'), '{}/tr_show_case.cgi?case_id={}'.format(url, case_id)))
                    self.println('%s %s' % (blue('Testcase automated:'), testcase['automated']))
                    self.println('%s %s' % (blue('Testcase status:'), testcase['status']))
                    self.println('%s %s' % (blue('Testcase requirements:'), testcase['requirement']))
                    if testcase['setup']:
                        self.println(blue('Testcase setup:'))
                        self.println(testcase['setup'])
                    if testcase['breakdown']:
                        self.println(blue('Testcase breakdown:'))
                        self.println(testcase['breakdown'])
                    self.println(blue('Testcase actions:'))
                    self.println(testcase['action'])
                    if testcase['effect']:
                        self.println(blue('Testcase effect:'))
                        self.println(testcase['effect'])

        else:
            self.parse_error(self.do_testopia_show, args)

    def complete_testopia_show(self, text, line, begidx, endidx):
        if not line.count(','):
            return self.complete_testopia_testcaselist(text, line, begidx, endidx)

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
            url = config.bugzilla_url
            testcase = {}
            fields = ['requirement:', 'setup:', 'breakdown:', 'action:', 'effect:']
            (package, _, summary) = args.partition(',')

            self.ensure_testopia_loaded()

            fields.insert(0, 'status: proposed')
            fields.insert(0, 'automated: no')
            fields.insert(0, 'package: %s' % package)
            fields.insert(0, 'summary: %s' % summary)

            edited = edit_text('\n'.join(fields))

            if edited == '\n'.join(fields):
                out.warning('testcase was not modified. not uploading.')
                return

            template = edited.replace('\n', '|br|')

            for field in fields:
                template = template.replace('|br|%s:' % field.partition(':')[0], '\n%s:' % field.partition(':')[0])

            lines = template.split('\n')
            for line in lines:
                key, _, value = line.partition(':')
                if key == 'package':
                    key = 'tags'
                    value = 'packagename_{name},testcase_{name}'.format(name=value.strip())

                testcase[key] = value.strip()

            try:
                case_id = self.testopia.create_testcase(testcase)
            except Exception:
                out.error('failed to create testcase')
            else:
                out.info('created testcase %s/tr_show_case.cgi?case_id=%s' % (url, case_id))

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
            url = config.bugzilla_url
            fields = ['summary', 'automated', 'status', 'requirement', 'setup', 'breakdown', 'action', 'effect']

            self.ensure_testopia_loaded()

            case = args.replace('_', ' ')
            try:
                case_id = str(int(case))
            except ValueError:
                try:
                    case_id = [ k for k, v in self.testopia.testcases.items() if v['summary'].replace('_', ' ') in case ][0]
                except IndexError:
                    out.critical('case_id for testcase %s not found' % case)
                    return

            testcase = self.testopia.get_testcase(case_id)

            if not testcase:
                return

            for field in fields:
                template.append('%s: %s' % (field, testcase[field]))

            edited = edit_text('\n'.join(template))

            if edited == '\n'.join(template):
                out.warning('testcase was not modified. not uploading.')
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
                out.error('failed to modify testcase %s' % case_id)
            else:
                out.info('testcase saved: %s/tr_show_case.cgi?case_id=%s' % (url, case_id))
        else:
            self.parse_error(self.do_testopia_edit, args)

    def complete_testopia_edit(self, text, line, begidx, endidx):
        if not line.count(','):
            return self.complete_testopia_testcaselist(text, line, begidx, endidx)

    @requires_update
    def do_list_bugs(self, args):
        """
        Lists related bugs and corresponding Bugzilla URLs.

        list_bugs
        Keyword arguments:
        None
        """

        if args:
            self.parse_error(self.do_list_bugs, args)
            return

        buglist = ','.join(sorted(self.metadata.bugs.keys()))

        url = config.bugzilla_url

        self.println('Buglist: {}/buglist.cgi?bug_id={}'.format(url, buglist))
        for (bug, description) in self.metadata.bugs.items():
            self.println()
            self.println('Bug #{0:5}: {1}'.format(bug, description))
            self.println('{}/show_bug.cgi?id={}'.format(url, bug))

    @requires_update
    def do_list_metadata(self, args):
        """
        Lists patchinfo metadata like patch number, SWAMP ID or packager.

        list_metadata
        Keyword arguments:
        None
        """

        self.metadata.show_yourself(self.sys.stdout)

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

        targets, params = self._parse_args(args, set)

        if params:
            packages = ' '.join(params)
        else:
            packages = ' '.join(self.metadata.get_package_list())

        history = {}

        if targets:
            if int(self.metadata.get_release()) > 10:
                query = "zypper se -s --match-exact -t package %s | egrep ^[iv] | awk -F '|' '{ print $2 $4 }' | uniq" % packages
            else:
                query = "zypper se --match-exact -t package %s | egrep ^[iv] | awk -F '|' '{ print $4 $5 }' | uniq" % packages

            RunCommand(targets, query).run()

            for target in targets:
                try:
                    checksum = reduce(lambda x, y: x + y, map(ord, targets[target].lastout()))
                except TypeError:
                    continue

                try:
                    history[checksum].append(target)
                except KeyError:
                    history[checksum] = []
                    history[checksum].append(target)

            for path in history:
                name = ''
                release = {}

                if len(history) > 1:
                    self.println('version history from:')
                    for target in history[path]:
                        self.println('  {} ({})'.format(target, targets[target].system))
                self.println()

                lines = targets[target].lastout().split('\n')
                for line in lines:
                    match = re.search('([^\s]+)\s+([^\s]+)', line)
                    if match:
                        name = match.group(1)
                        try:
                            release[name].append(match.group(2))
                        except KeyError:
                            release[name] = []
                            release[name].append(match.group(2))

                for package in release:
                    self.println('{}:'.format(package))
                    indent = 0
                    for version in sorted(release[package], key=RPMVersion, reverse=True):
                        self.println('  ' * indent + '-> {}'.format(version))
                        indent = indent + 1
                    self.println()

    def do_show_log(self, args):
        """
        Prints the command protocol from the specified hosts. This might be
        handy for the tester, as one can simply dump the command history to
        the reproducer section of the template.

        show_log <hostname>
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        """

        if args:
            targets, _ = self._parse_args(args, None)

            output = []

            for host in sorted(targets.values()):
                output.append('log from %s:' % host.hostname)
                for line in host.log:
                    output.append('%s:~> %s [%s]' % (host.hostname, line[0], line[3]))
                    output.append('stdout:')
                    map(output.append, line[1].split('\n'))
                    output.append('stderr:')
                    map(output.append, line[2].split('\n'))

            page(output, self.interactive)

        else:

            self.parse_error(self.do_show_log, args)

    def complete_show_log(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    def do_shell(self, args):
        """
        Invokes a remote root shell on the target host.
        The terminal size is set once, but isn't adapted on subsequent changes.

        shell <hostname>
        Keyword arguments:
        hostname -- hostname from the target list
        """

        if args:
            targets, _ = self._parse_args(args, None)

            for target in targets.keys():
                targets[target].shell()

        else:
            self.parse_error(self.do_shell, args)

    def complete_shell(self, text, line, begidx, endidx):
        if not line.count(','):
            return self.complete_hostlist(text, line, begidx, endidx)

    def do_run(self, args):
        """
        Runs a command on a specified host or on all enabled targets if
        'all' is given as hostname. The command timeout is set to 5 minutes
        which means, if there's no output on stdout or stderr for 5 minutes,
        a timeout exception is thrown. The commands are run in parallel on
        every target or in serial mode when set with "set_host_state".
        After the call returned, the output (including the return code)
        of each host is shown on the console.
        Please be aware that no interactive commands can be run with this
        procedure.

        run <hostname[,hostname,...],command>
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        """

        if not args:
            self.parse_error(self.do_run, args)

        targets, command = self._parse_args(args, str)

        for target in targets.keys():
            lock = targets[target].locked()
            if lock.locked and lock.comment and not lock.own():
                out.critical('host %s is exclusively locked by %s (%s). skipping.' % (target, lock.user, lock.comment))
                del targets[target]

        if targets:
            try:
                RunCommand(targets, command).run()
            except KeyboardInterrupt:
                return

            output = []

            for target in targets:
                output.append('%s:~> %s [%s]' % (target, targets[target].lastin(), targets[target].lastexit()))
                map(output.append, targets[target].lastout().split('\n'))
                if targets[target].lasterr():
                    map(output.append, ['stderr:'] + targets[target].lasterr().split('\n'))

            page(output, self.interactive)
            out.info('done')

    def complete_run(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    def do_testsuite_list(self, args):
        """
        List available testsuites on the target hosts.

        testsuite_list <hostname>
        Keyword arguments:
        hostname   -- hostname from the target list or "all"
        """

        if args:
            path = config.target_testsuitedir

            targets, _ = self._parse_args(args, None)

            for target in targets:
                self.println('testsuites on {} ({}):'.format(target, targets[target].system))
                self.println('\n'.join([i for i in sorted(targets[target].listdir(path)) if i.endswith('-run')]))
                self.println()
        else:
            self.parse_error(self.do_testsuite_list, args)

    def complete_testsuite_list(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    @requires_update
    def do_testsuite_run(self, args):
        """
        Runs ctcs2 testsuite and saves logs to /var/log/qa/$md5 on the
        target hosts. Results can be submitted with the testsuite_submit
        command.

        testsuite_run <hostname>[,hostname,...],<testsuite>
        Keyword arguments:
        hostname   -- hostname from the target list or "all"
        testsuite  -- testsuite-run command
        """

        targets, command = self._parse_args(args, str)

        if not (targets and command):
            self.parse_error(self.do_testsuite_run, args)
            return

        if not command.startswith('/'):
            command = os.path.join(config.target_testsuitedir, command.strip())

        command = 'export TESTS_LOGDIR=/var/log/qa/{0}; {1}'.format(
            self.metadata.id,
            command
        )
        name = os.path.basename(command).replace('-run', '')

        try:
            RunCommand(targets, command).run()
        except KeyboardInterrupt:
            out.info('testsuite run canceled')
            return

        for target in targets:
            self.println('{}:~> {}-testsuite [{}]'.format(target, name, targets[target].lastexit()))
            self.println(targets[target].lastout())
            if targets[target].lasterr():
                self.println(targets[target].lasterr())

        out.info('done')

    def complete_testsuite_run(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    @requires_update
    def do_testsuite_submit(self, args):
        """
        Submits the ctcs2 testsuite results to qadb.suse.de.
        The comment field is populated with some attributes like SWAMPID or
        testsuite name, but can also be edited before the results get
        submitted. Submitting results to qadb requires the rd-qa NIS
        password.

        testsuite_submit <hostname>,hostname,...,<testsuite>
        Keyword arguments:
        hostname   -- hostname from the target list or "all"
        testsuite  -- testsuite-run command
        """

        targets, command = self._parse_args(args, str)

        if not (targets and command):
            self.parse_error(self.do_testsuite_submit, args)
            return

        name = os.path.basename(command).replace('-run', '')
        username = config.session_user

        comment = self.metadata.get_testsuite_comment(name)
        comment.edit_text()

        out.info('please specify rd-qa NIS password')
        password = getpass.getpass()

        submit = []
        submit.append('echo \'echo -n "%s"\' > /tmp/pwdask' % password)
        submit.append('chmod 700 /tmp/pwdask')
        submit.append('SSH_ASKPASS=/tmp/pwdask DISPLAY=dummydisplay:0 /usr/share/qa/tools/remote_qa_db_report.pl -b -t patch:{0} -T {1} -f /var/log/qa/{0} -c \'{2}\''.format(
            self.metadata.id,
            username,
            comment
        ))
        submit.append('rm /tmp/pwdask')

        for target in targets:
            for command in submit:
                try:
                    temp = {target:targets[target]}
                    RunCommand(temp, command).run()
                except KeyboardInterrupt:
                    return

                if 'remote_qa_db_report.pl' in command:
                    if targets[target].lastexit() != 0:
                        out.critical('submitting testsuite results failed on %s:' % target)
                        self.println('{}:~> {} [{}]'.format(target, name, targets[target].lastexit()))
                        self.println(targets[target].lastout())
                        if targets[target].lasterr():
                            self.println(targets[target].lasterr())
                    else:
                        match = re.search('(http://.*/submission.php.submission_id=\d+)', targets[target].lasterr())
                        if match:
                            system = targets[target].system
                            out.info('submission for %s (%s): %s' % (target, system, match.group(1)))
                        else:
                            out.critical('no submission found for %s. please use "show_log %s" to see what went wrong' % (target,
                                         target))

        out.info('done')

    def complete_testsuite_submit(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    def do_set_session_name(self, args):
        """
        Set optional mtui session name as part of the prompt string.
        This should help finding the corrent mtui session if multiple
        sessions are active.

        set_session_name [name]
        Keyword arguments:
        name     -- session name
        """

        session = args.strip()
        if not session:
            session = self.metadata.id

        self.set_prompt(session)
        self.session = session

    def set_prompt(self, session=None):
        self.session = session
        session = ":"+str(session) if session else ''
        self.prompt = 'mtui{0}> '.format(session)

    def do_load_template(self, args):
        """
        Load QA Maintenance template by md5 identifier. All changes and logs
        from an already loaded template are lost if not saved previously.
        Already connected hosts are kept and extended by the reference hosts
        defined in the template file.

        load_template <update_id>
        Keyword arguments:
        update_id      -- either md5sum for swamp update or
                          obs request review id for obs update
        """

        id_ = args.strip()
        update = None
        u_types = [SwampUpdateID, OBSUpdateID]
        for i in u_types:
            try:
                update = i(id_)
            except ValueError as e:
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

        self.load_update(update)

        for hostname, system in re_add:
            self.connect_system_if_unconnected(hostname, system)

    def load_update(self, update, autoconnect=True):
        update.config = self.config
        update.log = self.log

        tr = update.make_testreport()

        if autoconnect:
            tr.load_systems_from_testplatforms()
            tr.connect_targets()

        if self.metadata and self.metadata.id is self.session:
            self.set_prompt(None)
        self.metadata = tr
        self.targets = tr.targets

    def do_set_location(self, args):
        """
        Change current reference host location to another site.

        set_location <site>
        Keyword arguments:
        site     -- location name
        """

        args = args.strip()
        if not args:
            self.parse_error(self.do_set_location, args)
            return

        old = self.config.location
        self.config.location = args
        self.log.info(messages.LocationChangedMessage(old, args))

    def complete_set_location(self, text, line, begidx, endidx):
        refhost = self._refhosts()
        return [i for i in refhost.get_locations() if i.startswith(text) and i not in line]

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
                        out.warning('host %s is locked since %s by %s. skipping.' % (target, lock.time(), lock.user))
                        if lock.comment:
                            out.info("%s's comment: %s" % (lock.user, lock.comment))

                        continue
                    else:
                        targets[target].set_locked(comment)
                elif state == 'disabled':
                    husv = StrictVersion(commands.HostsUnlock.stable)
                    if husv <=  self._interface_version:
                        msg = "set_host_lock <host>,disable has been"
                        msg += " deprecated in favor of unlock command"
                        user_deprecation(out, msg)
                    try:
                        targets[target].remove_lock()
                    except AssertionError:
                        out.warning('host %s not locked by us. skipping.' % target)
                else:
                    self.parse_error(self.do_set_host_lock, args)
        else:

            self.parse_error(self.do_set_host_lock, args)
            return

    def complete_set_host_lock(self, text, line, begidx, endidx):
        if line.count(','):
            return self.complete_enabled_hostlist(text, line, begidx, endidx, ['enabled', 'disabled'])
        else:
            return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

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
            return self.complete_hostlist(text, line, begidx, endidx, ['enabled', 'disabled', 'dryrun', 'serial', 'parallel'])
        else:
            return self.complete_hostlist_with_all(text, line, begidx, endidx)

    def do_set_log_level(self, args):
        """
        Changes the current default MTUI loglevel "info" to "warning"
        or "debug". To enable debug messages, one can set the loglevel
        to "debug". This could be handy for longer running commands as
        the output is shown in realtime. The "warning" loglevel prints
        just basic error or warning conditions. Therefore it's not
        recommended to use the "warning" loglevel.

        set_log_level <loglevel>
        Keyword arguments:
        loglevel   -- warning, info or debug
        """

        levels = {'warning': logging.WARNING, 'info': logging.INFO, 'debug': logging.DEBUG}

        if args in levels.keys():
            out.setLevel(level=levels[args])
        else:
            self.parse_error(self.do_set_log_level, args)

    def complete_set_log_level(self, text, line, begidx, endidx):
        return [i for i in ['warning', 'info', 'debug'] if i.startswith(text) and i not in line]

    def do_set_timeout(self, args):
        """
        Changes the current execution timeout for a target host.
        When the timeout limit was hit the user is asked to wait
        for the current command to return or to proceed with the
        next one.
        The timeout value is set in seconds. To disable the
        timeout set it to "0".

        set_timeout <hostname,timeout>
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        timeout  -- timeout value in seconds
        """

        targets, timeout = self._parse_args(args, str)

        if targets and timeout:
            try:
                value = int(timeout)
            except Exception:
                out.error('wrong timeout value: %s' % timeout)
                self.parse_error(self.do_set_timeout, args)
                return

            for target in targets:
                targets[target].set_timeout(value)
        else:

            self.parse_error(self.do_set_timeout, args)

    def complete_set_timeout(self, text, line, begidx, endidx):
        return self.complete_hostlist_with_all(text, line, begidx, endidx)

    def do_set_repo(self, args):
        """
        Sets the software repositories to UPDATE or TESTING. Multiple
        hostnames can be given. On the target hosts, the rep-clean.sh script
        is spawned to set the repositories accordingly.

        set_repo <hostname>[,hostname,...],<repository>
        Keyword arguments:
        hostname   -- hostname from the target list or "all"
        repository -- repository, TESTING or UPDATE
        """

        targets, name = self._parse_args(args, str)

        if not (targets and name):
            self.parse_error(self.do_set_repo, args)
            return

        with LockedTargets([self.targets[x] for x in targets]):
            for t in [self.targets[x] for x in targets]:
                t.set_repo(name.upper(), self.metadata)

    def complete_set_repo(self, text, line, begidx, endidx):
        if line.count(','):
            return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx, ['testing', 'update'])
        else:
            return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    @requires_update
    def do_install(self, args):
        """
        Installs packages from the current active repository.
        The repository should be set with the set_repo command beforehand.

        install <hostname>[,hostname,...],<package>[ package ...]
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        package  -- package name
        """

        targets, packages = self._parse_args(args, str)

        if not (targets and packages):
            self.parse_error(self.do_install, args)
            return

        if targets:
            out.info('installing')
            try:
                self.metadata.perform_install(targets, packages.split())
            except Exception:
                out.critical('failed to install packages')
                return
            except KeyboardInterrupt:
                out.info('installation process canceled')
                return
            else:
                out.info('done')

    def complete_install(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    @requires_update
    def do_uninstall(self, args):
        """
        Removes packages from the system.

        uninstall <hostname>[,hostname,...],<package>[ package ...]
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        package  -- package name
        """

        targets, packages = self._parse_args(args, str)

        if not (targets and packages):
            self.parse_error(self.do_uninstall, args)
            return

        if targets:
            out.info('removing')
            try:
                self.metadata.perform_uninstall(targets, packages.split())
            except Exception:
                out.critical('failed to remove packages')
                return
            except KeyboardInterrupt:
                out.info('uninstallation process canceled')
                return
            else:
                out.info('done')

    def complete_uninstall(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    @requires_update
    def do_downgrade(self, args):
        """
        Downgrades all related packages to the last released version (using
        the UPDATE channel). This does not work for SLES 9 hosts, though.

        downgrade <hostname>
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        """

        targets, _ = self._parse_args(args, None)

        if (not targets) or _:
            self.parse_error(self.do_downgrade, args)
            return

        if targets:
            out.info('downgrading')
            try:
                self.metadata.perform_downgrade(targets)
            except Exception:
                out.critical('failed to downgrade target systems')
                return
            except KeyboardInterrupt:
                out.info('downgrade process canceled')
                return
            else:
                out.info('done')

    def complete_downgrade(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    @requires_update
    def do_prepare(self, args):
        """
        Installs missing or outdated packages from the UPDATE repositories.
        This is also run by the update procedure before applying the updates.
        If "force" is set, packages are forced to be installed on package
        conflicts. If "installed" is set, only installed packages are
        prepared. If "testing" is set, packages are installed from the TESTING
        repositories.

        prepare <hostname>[,hostname,...][,force][,installed][,testing]
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        """

        targets, params = self._parse_args(args, set)

        if not targets:
            self.parse_error(self.do_prepare, args)
            return

        opts = dict(
            force = False,
            installed = False,
            testing = False,
        )

        for opt in opts:
            if opt in params:
                opts[opt] = True

        self._do_prepare_impl(targets, **opts)

    def _do_prepare_impl(self, targets, force = False, installed = False, testing = False):
        if targets:
            out.info('preparing')

            try:
                self.metadata.perform_prepare(
                    targets,
                    force = force,
                    installed_only = installed,
                    testing = testing
                )
            except Exception:
                out.critical('failed to prepare target systems')
                out.debug(format_exc())
                return False
            except KeyboardInterrupt:
                out.info('preparation process canceled')
                return False
            else:
                out.info('done')

    def complete_prepare(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx, ['force', 'installed', 'testing'])

    @requires_update
    def do_update(self, args):
        """
        Applies the testing update to the target hosts. While updating the
        machines, the pre-, post- and compare scripts are run before and
        after the update process. If the update adds new packages to the
        channel, the "newpackage" parameter triggers the package installation
        right after the update. To skip the preparation procedure, append
        "noprepare" to the argument list.

        update <hostname>[,newpackage][,noprepare][,noscript]
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        """

        targets, params = self._parse_args(args, set)

        if not targets:
            self.parse_error(self.do_update, args)
            return

        prepare = dict()

        # don't install new packages when doing a noninteractive kernel update
        if not self.interactive and [x for x in self.metadata.packages if x in ['-kmp-', 'kernel-default']]:
            if 'newpackage' in params:
                params.remove('newpackage')
            prepare['installed'] = True

        if 'noprepare' not in params:
            if self._do_prepare_impl(targets, **prepare) is False:
                return

        with LockedTargets([self.targets[x] for x in targets]):
            missing = False
            for target in targets:
                not_installed = []
                packages = targets[target].packages

                targets[target].query_versions()

                for package in packages:
                    required = self.metadata.packages[package]
                    before = targets[target].packages[package].current

                    packages[package].set_versions(before=before, required=required)

                    if before is None or before == '0':
                        missing = True
                        not_installed.append(package)
                    else:
                        if RPMVersion(before) >= RPMVersion(required):
                            out.warning('%s: package is too recent: %s (%s, target version is %s)' % (target, package, before, required))

                if len(not_installed):
                    out.warning('%s: these packages are not installed: %s' % (target, not_installed))

            if missing:
                out.warning('%s: these packages are missing: %s' % (target, not_installed))

            if 'noscript' not in params:
                self.metadata.script_hooks(PreScript).run(targets.values())

            out.info('updating')

            try:
                self.metadata.perform_update(targets)
            except Exception:
                out.critical('failed to update target systems')
                out.debug(format_exc())
                Notification('MTUI', 'updating %s failed' % self.session, 'stock_dialog-error').show()
                raise
            except KeyboardInterrupt:
                out.info('update process canceled')
                return

            if 'newpackage' in params:
                self._do_prepare_impl(targets, testing = True, **prepare)

            for target in targets:
                packages = targets[target].packages

                targets[target].query_versions()

                for package in packages:
                    before = packages[package].before
                    required = packages[package].required
                    after = targets[target].packages[package].current

                    packages[package].set_versions(after=after)

                    if after is not None and after != '0':
                        if RPMVersion(before) == RPMVersion(after):
                            out.warning('%s: package was not updated: %s (%s)' % (target, package, after))

                        if RPMVersion(after) < RPMVersion(required):
                            out.warning('%s: package does not match required version: %s (%s, required %s)' % (target, package, after,
                                        required))

            if 'noscript' not in params:
                self.metadata.script_hooks(PostScript).run(targets.values())
                self.metadata.script_hooks(CompareScript).run(targets.values())
                FileDelete(targets.values(), self.metadata.target_wd('output')).run()

        Notification('MTUI', 'updating %s finished' % self.session).show()
        out.info('done')

    def complete_update(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx, ['newpackage', 'noprepare', 'noscript'])

    def do_list_sessions(self, args):
        """
        Lists current active ssh sessions on target hosts.

        list_sessions <hostname>
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        """

        command = "ss -r  | sed -n 's/^[^:]*:ssh *\([^ ]*\):.*/\\1/p' | sort -u"

        targets, _ = self._parse_args(args, None)

        if targets:
            try:
                RunCommand(targets, command).run()
            except KeyboardInterrupt:
                return

        for host in sorted(targets.values()):
            self.println('sessions on {} ({}):'.format(host.hostname, host.system))
            self.println(host.lastout())

    def complete_list_sessions(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    @requires_update
    def do_checkout(self, args):
        """
        Update template files from the SVN.

        checkout
        Keyword arguments:
        none
        """

        try:
            subprocess.check_call('svn up'.split(), cwd = self.metadata.report_wd())
        except Exception:
            out.error('updating template failed')
            out.debug(format_exc())

    @requires_update
    def do_commit(self, args):
        """
        Commits the testing template to the SVN. This can be run after the
        testing has finished an the template is in the final state.

        commit [message]
        Keyword arguments:
        message  -- commit message
        """

        msg = ['-m', args] if args else []

        checkout = self.metadata.report_wd()
        try:
            subprocess.check_call('svn up'.split(), cwd = checkout)
            subprocess.check_call('svn ci'.split() + msg, cwd = checkout)
        except Exception:
            out.error('committing template failed')
            out.debug(format_exc())

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

            FileUpload(self.targets.values(), filename, remote).run()
            self.log.info('uploaded {0} to {1}'.format(filename, remote))

    def complete_put(self, text, line, begidx, endidx):
        return self.complete_filelist(text, line, begidx, endidx)

    def do_get(self, args):
        """
        Downloads a file from all enabled hosts. Multiple files can not be
        selected. Files are saved in the $TEMPLATE_DIR/downloads/ subdirectory
        with the hostname as file extension.

        get <remote filename>
        Keyword arguments:
        filename -- file to download from the target hosts
        """

        if not args:
            self.parse_error(self.do_get, args)
            return

        local = self.metadata.downloads_wd(os.path.basename(args), filepath=True)

        FileDownload(self.targets.values(), args, local, True).run()
        self.log.info('downloaded {0} to {1}'.format(args, local))

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
                    out.error('running %s failed' % filename)
                    out.debug(format_exc())
            else:
                out.error('%s script not found, make sure term.%s.sh exists' % (args, args))
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
        edit specfile
        edit patch,<patchname>
        Keyword arguments:
        filename -- edit filename
        template -- edit template
        specfile -- edit specfile
        patch    -- edit patch
        """

        (command, _, filename) = args.partition(',')

        editor = os.environ.get('EDITOR', 'vi')

        # all but the file command needs template data. skip if template
        # isn't loaded
        if not self.metadata and command != 'file':
            out.error('no testing template loaded')
            return

        if command == 'file':
            path = filename
        elif command == 'template':
            path = self.metadata.path
        elif command in ['specfile', 'patch']:
            if command == 'specfile':
                filename = '*.spec'
            path = os.path.join(self.metadata.local_wd(), '*', filename)
            if not glob.glob(path):
                self.metadata.extract_source_rpm()
        else:
            self.parse_error(self.do_edit, args)
            return

        try:
            subprocess.check_call([editor, path])
        except Exception:
            out.error("failed to run %s" % editor)
            out.debug(format_exc())

    def complete_edit(self, text, line, begidx, endidx):
        if 'file,' in line:
            return self.complete_filelist(text.replace('file,', '', 1), line, begidx, endidx)
        if 'patch,' in line:
            specfile = glob.glob(self.metadata.local_wd(), '*', '*.spec')
            with open(specfile, 'r') as spec:
                name = re.findall('Name:\W+(.*)', spec.read())[0]
                spec.seek(0)
                return [i for i in [s.replace('name}', name) for s in re.findall('Patch\d*:\W+(.*)', spec.read())] if i.startswith(text)]
        else:
            return [i for i in ['file,', 'template', 'specfile', 'patch,'] if i.startswith(text)]

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

        targets = self.targets

        output = XMLOutput()
        output.add_header(self.metadata)

        for target in targets:
            output.add_target(targets[target])

        try:
            template = xml_to_template(self.metadata.path, output.pretty(), hostname)
        except Exception:
            out.error('failed to export XML')
            return

        if os.path.exists(filename) and not force:
            out.warning('file %s exists.' % filename)
            if not prompt_user('should i overwrite %s? (y/N) ' % filename, ['y', 'yes'], self.interactive):
                filename += '.' + timestamp()

        out.info('exporting XML to %s' % filename)
        try:
            with open(filename, 'w') as f:
                f.write('\n'.join(l.rstrip().encode('utf-8') for l in template))
        except IOError as error:
            self.println('failed to write {}: {}'.format(filename, error.strerror))
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

    def _do_save_impl(self, path = 'log.xml'):
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

        output = XMLOutput()
        if self.metadata:
            output.add_header(self.metadata)

        for target in self.targets.values():
            output.add_target(target)

        with open(path, 'w') as f:
            f.write(output.pretty())

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

        sys.exit(0)

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

        return [dirname + i for i in os.listdir(dirname) if i.startswith(filename)]

    def complete_hostlist(self, text, line, begidx, endidx, appendix=[]):
        return [i for i in list(self.targets) + appendix if i.startswith(text) and i not in line]

    def complete_hostlist_with_all(self, text, line, begidx, endidx, appendix=[]):
        return [i for i in list(self.targets) + ['all'] + appendix if i.startswith(text) and i not in line]

    def complete_enabled_hostlist(self, text, line, begidx, endidx, appendix=[]):
        return [i for i in list(self.targets.select(enabled = True)) + appendix if i.startswith(text) and i not in line]

    def complete_enabled_hostlist_with_all(self, text, line, begidx, endidx, appendix=[]):
        return [i for i in list(self.targets.select(enabled = True)) + ['all'] + appendix if i.startswith(text) and i not in line]

    def complete_packagelist(self, text, line, begidx, endidx, appendix=[]):
        return [i for i in self.metadata.get_package_list() if i.startswith(text) and i not in line]

    def complete_testopia_testcaselist(self, text, line, begidx, endidx):
        self.ensure_testopia_loaded()

        testcases = [ i['summary'].replace(' ', '_') for i in self.testopia.testcases.values() ]
        return [i for i in testcases if i.startswith(text) and i not in line]

    def parse_error(self, method, args):
        self.println()
        out.error('failed to parse command: %s %s' % (method.__name__.replace('do_', ''), args))
        self.println('{}: {}'.format(method.__name__.replace('do_', ''), method.__doc__))

class Script(object):
    """
    :type subdir: str
    :param subdir: subdirectory in the L{TestReport.scripts_wd} where the
        scripts are located.

        Note: also used as a "type of the script" and can be shown to
        the user.

    FIXME: should be an abstract attribute
    """

    def __init__(self, tr, path, log, file_uploader, cmd_runner):
        """
        :type path: str
        :param path: absolute path to the script
        """
        self.path = path
        self.name = basename(path)
        self.testreport = tr
        self.log = log
        self.file_uploader = file_uploader
        self.cmd_runner = cmd_runner

    def __repr__(self):
        return "<{0}.{1} {2} for {3}>".format(
            self.__module__,
            self.__class__.__name__,
            self.path,
            repr(self.testreport)
        )

    def __str__(self):
        return "{0} script {1}".format(
            self.subdir,
            self.name,
        )

    @classmethod
    def absolute_subdir(cls, tr):
        """
        :type tr: L{TestReport}
        """
        return tr.scripts_wd(cls.subdir)

    def run(self, targets):
        """
        :type targets: [L{Target}]
        """
        ass_isL(targets, TargetI)

        try:
            self.log.info('running {0}'.format(self))
            self._run(targets)
        except KeyboardInterrupt:
            self.log.warning('skipping {0}'.format(self))
            return

    def results_wd(self, *path, **kw):
        return self.testreport.report_wd(
            'output',
            'scripts',
            *path,
            **kw
        )

    def _filename(self, target = None, subdir = None):
        """
        :returns: str "fully qualified" file name
        """
        ass_is(target, TargetI, True)

        if not subdir:
            subdir = self.subdir

        xs = [subdir, splitext(self.name)[0]]
        if target:
            xs.append(target.hostname)

        return ".".join(xs)

class PreScript(Script):
    subdir = "pre"

    def remote_path(self):
        return self.testreport.target_wd(self._filename())

    def result_file(self, target):
        """
        :type target: L{TargetI} instance
        """
        ass_is(target, TargetI)
        return self.results_wd(self._filename(target), filepath = True)

    def remote_pkglist_path(self):
        return self.testreport.target_wd('package-list.txt')

    def _run(self, targets):
        ass_isL(targets, TargetI)

        self.file_uploader(
            targets,
            self.path,
            self.remote_path(),
        ).run()

        self.file_uploader(
            targets,
            self.testreport.pkg_list_file(),
            self.remote_pkglist_path(),
        ).run()

        self.cmd_runner(
            dict([(t.hostname, t) for t in targets]),
            self.mk_command()
        ).run()

        for t in targets:
            fname = self.result_file(t)
            try:
                with open(fname, 'w') as f:
                    f.write(t.lastout())
                    f.write(t.lasterr())
            except IOError as e:
                self.log.error(messages.FailedToWriteScriptResult(fname, e))

    def mk_command(self):
        return "{exe} -r {repository} -p {pkg_list_file} {id}".format(
            exe = self.remote_path(),
            repository = self.testreport.repository,
            pkg_list_file = self.remote_pkglist_path(),
            id  = self.testreport.id,
        )

class PostScript(PreScript):
    subdir = "post"

class CompareScript(Script):
    subdir = "compare"

    def _run(self, ts):
        ass_isL(ts, TargetI)
        for t in ts:
            self._run_single_target(t)

    def _result(self, s, t):
        return self.results_wd(self._filename(
                subdir = s,
                target = t,
            ).replace("compare_", "check_"),
            filepath = True
        )

    def _pre_file(self, t):
        return self._result(PreScript.subdir, t)

    def _post_file(self, t):
        return self._result(PostScript.subdir, t)

    def _run_single_target(self, t):
        ass_is(t, TargetI)

        argv = [
            self.path,
            self._pre_file(t),
            self._post_file(t),
        ]

        self.log.debug("running {0}".format(argv))
        stdout = stderr = None
        try:
            p = subprocess.Popen(
                argv,
                stdout = subprocess.PIPE,
                stderr = subprocess.PIPE
            )
        except Exception as e:
            t.log.append([' '.join(argv), '', '', 0x100, 0])
            self.log.critical(messages.StartingCompareScriptError(e, argv))
            return

        (stdout, stderr) = p.communicate()
        rc = p.wait()

        t.log.append([' '.join(argv), str(stdout), str(stderr), rc, 0])

        if rc == 0:
            return

        if rc == 2:
            logger, msg = self.log.critical, messages.CompareScriptCrashed
        else:
            logger, msg = self.log.warning, messages.CompareScriptFailed

        assert callable(logger), "{0!r} not callable".format(logger)

        logger(msg(argv, stdout, stderr, rc))


def user_deprecation(out, msg):
    out.warning(msg)
