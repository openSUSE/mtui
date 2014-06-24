# -*- coding: utf-8 -*-
#
# mtui command line prompt
#

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

from datetime import date, datetime
from traceback import print_exc

from mtui.rpmver import *
from mtui.target import *
from mtui.template import TestReportFactory
from mtui.updater import *
from mtui.export import *
from mtui.utils import *
from mtui.refhost import *
from mtui.config import *
from mtui.notification import *
from mtui.testopia import *
from mtui import commands, strict_version
from mtui.utils import log_exception
from mtui.commands import ArgsParseFailure

from distutils.version import StrictVersion

out = logging.getLogger('mtui')

class QuitLoop(RuntimeError):
    pass

class CmdQueue(list):
    """
    Prerun support.

    Echos prompt with the command that's being popped (and about to be
    executed
    """
    def __init__(self, iterable, prompt):
        self.prompt = prompt
        list.__init__(self, iterable)

    def pop(self, i):
        val = list.pop(self, i)
        self.echo_prompt(val)
        return val

    def echo_prompt(self, val):
        print "{0}{1}".format(self.prompt, val)

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
    prompt = 'mtui> '

    def __init__(self, targets, metadata, config, log):
        """
            :param targets: dict where K is str, V is L{Target} and
                K == V.hostname
        """
        cmd.Cmd.__init__(self)
        self.interactive = True
        self.targets = targets
        self.metadata = metadata
        self.homedir = os.path.expanduser('~')
        self.config = config
        self.log = log
        self.datadir = self.config.datadir

        self.set_interface_version(config.interface_version)

        self.session = self.metadata.md5

        self.testopia = None

        readline.set_completer_delims('`!@#$%^&*()=+[{]}\|;:",<>? ')

        self._read_history()

        self.commands = {}
        self._add_subcommand(commands.HostsUnlock)
        self._add_subcommand(commands.Whoami)
        self._add_subcommand(commands.Config)

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
        if self.commands.has_key(cmd.command):
            raise RuntimeError("command {0} already set".\
                format(cmd.command))

        if self._interface_version < StrictVersion(cmd.stable):
            return

        self.commands[cmd.command] = cmd

    def set_cmdqueue(self, queue):
        self.cmdqueue = CmdQueue(queue, self.prompt)

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
                self.set_cmdqueue([])
                # make the new prompt to be printed on new line
                print ""
            except QuitLoop:
                return

    # {{{ overrides to support new style commands
    def onecmd(self, line):
        cmd_, arg, line = self.parseline(line)
        try:
            subcmd = self.commands[cmd_]
        except KeyError:
            return cmd.Cmd.onecmd(self, line)

        try:
            args = subcmd.parse_args(arg, self.stdout)
        except ArgsParseFailure as e:
            return

        self.commandFactory(subcmd, args).run()

    def _hostsGroupFactory(self):
        """
        :returns: L{HostsGroup} consisting of enabled hosts only
        """
        return HostsGroup(enabled_targets(self.targets).values())

    def commandFactory(self, cmd, args=None):
        hosts = self._hostsGroupFactory()
        return cmd(args, hosts, self.config, self.stdout, self.log, self)

    def do_help(self, arg):
        try:
            cmd_ = self.commands[arg]
        except KeyError:
            return cmd.Cmd.do_help(self, arg)
        else:
            cmd_.argparser(self.stdout).print_help()

    def get_names(self):
        names = cmd.Cmd.get_names(self)
        names = names + ["do_" + x for x in self.commands.keys()]
        return names

    def __getattr__(self, x):
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
                    (c.completer(self._hostsGroupFactory()))
        else:
            argparser = c.argparser(self.stdout)
            clsdict = {
                '__doc__': argparser.format_help()
            }
            return type(x, (object,), clsdict)
    # }}}

    def emptyline(self):
        return

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

        if args:
            attributes = Attributes()

            try:
                refhost = Refhost(config.refhosts_xml, self.metadata.location)
            except Exception:
                out.error('failed to load reference hosts data')
                return

            if 'Testplatform:' in args:
                # USECASE: this branch is handling a case where user loads mtui
                # without a testreport and copies the Testplatform: line
                # from some testreport into search_hosts or autoadd or
                # loading other set of hosts for running the current update
                # on
                try:
                    refhost.set_attributes_from_testplatform(args.replace('Testplatform: ', ''))
                    hosts = refhost.search()
                except (ValueError, KeyError):
                    hosts = []
                    out.error('failed to parse Testplatform string')
            elif refhost.get_host_attributes(args):
                hosts = [args]
            else:
                for _tag in args.split(' '):
                    tag = _tag.lower()
                    match = re.search('(\d+)\.(\d+)', tag)
                    if match:
                        attributes.major = match.group(1)
                        attributes.minor = match.group(2)
                    if tag in attributes.tags['products']:
                        attributes.product = tag
                    if tag in attributes.tags['archs']:
                        attributes.archs.append(tag)
                    if tag in attributes.tags['addons']:
                        attributes.addons.update({tag:{}})
                    if tag in attributes.tags['major']:
                        attributes.major = tag
                    if tag in attributes.tags['minor']:
                        attributes.minor = tag
                    if tag == 'kernel':
                        attributes.kernel = True
                    if tag == 'ltss':
                        attributes.ltss = True
                    if tag == 'minimal':
                        attributes.minimal = True
                    if tag == '!kernel':
                        attributes.kernel = False
                    if tag == '!ltss':
                        attributes.ltss = False
                    if tag == '!minimal':
                        attributes.minimal = False
                    if tag == 'xenu':
                        attributes.virtual.update({'mode':'guest', 'hypervisor':'xen'})
                    if tag == 'xen0':
                        attributes.virtual.update({'mode':'host', 'hypervisor':'xen'})
                    if tag == 'xen':
                        attributes.virtual.update({'hypervisor':'xen'})
                    if tag == 'kvm':
                        attributes.virtual.update({'hypervisor':'kvm'})
                    if tag == 'vmware':
                        attributes.virtual.update({'hypervisor':'vmware'})
                    if tag == 'host':
                        attributes.virtual.update({'mode':'host'})
                    if tag == 'guest':
                        attributes.virtual.update({'mode':'guest'})

                hosts = refhost.search(attributes)

                # check if some tags were passed to the attributes object which has
                # all archs set by default
                if not set(str(attributes).split()) ^ set(attributes.tags["archs"]):
                    return []

            for hostname in set(hosts):
                hosttags = refhost.get_host_attributes(hostname)
                print '{0:25}: {1}'.format(hostname, hosttags)

            return hosts

        else:
            self.parse_error(self.do_search_hosts, args)

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

        if args:
            try:
                refhost = Refhost(config.refhosts_xml, self.metadata.location)
            except Exception:
                out.error('failed to load reference hosts data')
                return

            hosts = self.do_search_hosts(args)

            for hostname in hosts:
                attributes = refhost.get_host_attributes(hostname)
                try:
                    out.warning('already connected to %s. skipping.' % self.targets[hostname].hostname)
                except KeyError:
                    try:
                        system = refhost.get_host_systemname(hostname)
                        self.targets[hostname] = Target(hostname, system, self.metadata.get_package_list())
                        self.metadata.systems[hostname] = system
                    except Exception:
                        out.error('failed to add host %s to list' % hostname)
        else:
            self.parse_error(self.do_autoadd, args)

    def complete_autoadd(self, text, line, begidx, endidx):
        attributes = Attributes()
        return [item for sublist in attributes.tags.values() for item in sublist if item.startswith(text) and item not in line]

    def do_add_host(self, args):
        """
        Adds another machine to the target host list. The system type needs
        to be specified as well.

        add_host <hostname,system>
        Keyword arguments:
        hostname -- address of the target host (should be the FQDN)
        system   -- system type, ie. sles11sp1-i386
        """

        if args:
            try:
                (hostname, system) = args.split(',')
            except ValueError:
                self.parse_error(self.do_add_host, args)
                return

            try:
                out.warning('already connected to %s. skipping.' % self.targets[hostname].hostname)
            except KeyError:
                try:
                    self.targets[hostname] = Target(hostname, system, self.metadata.get_package_list())
                    self.metadata.systems[hostname] = system
                except Exception:
                    out.error('failed to add host %s to list' % hostname)
        else:
            self.parse_error(self.do_add_host, args)

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

        if args:
            if args == 'all':
                [ self.targets[x].close() or self.targets.pop(x) for x in set(self.targets)]
            else:
                [ self.targets[x].close() or self.targets.pop(x) for x in set(args.split(',')) & set(self.targets)]
        else:
            self.parse_error(self.do_remove_host, args)

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
                print '{0:20} {1:20}: {2} ({3})'.format(host.hostname, system, state, mode)

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
            filters = ['connect', 'disconnect', 'install', 'update', 'downgrade']

            option = []
            parameters = args.split(',')
            [ option.append('-e ":%s"' % x) for x in set(parameters) & set(filters)]

            hosts = ','.join(set(parameters) & set(list(self.targets) + ['all']))

            count = 10
            targets = enabled_targets(self.targets)

            if hosts.split(',')[0] != 'all':
                count = 50
                targets = selected_targets(targets, hosts.split(','))

            if targets:
                if option:
                    RunCommand(targets, 'tac /var/log/mtui.log | grep -m %s %s | tac' % (count, ' '.join(option))).run()
                else:
                    RunCommand(targets, 'tail -n %s /var/log/mtui.log' % count).run()

            for host in sorted(targets.values()):
                print 'history from %s (%s):' % (host.hostname, host.system)
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
                    print '%s, %s: %s' % (time.strftime('%A, %d.%m.%Y %H:%M'), who, event)
                print
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

            targets = enabled_targets(self.targets)

            for host in sorted(targets.values()):
                system = '(%s)' % host.system
                lock = host.locked()
                if lock.locked:
                    if lock.own():
                        lockedby = 'me'
                    else:
                        lockedby = lock.user

                    print '{0:20} {1:20}: {2}'.format(host.hostname, system, yellow('since %s by %s' % (lock.time(), lockedby))),
                    if lock.comment:
                        print ': %s' % lock.comment
                    else:
                        print
                else:
                    print '{0:20} {1:20}: {2}'.format(host.hostname, system, green('not locked'))

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
                print '{0:20} {1:20}: {2}s'.format(host.hostname, system, timeout)

    def do_source_install(self, args):
        """
        Installs current source RPMs to the target hosts.

        source_install <hostname>
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        """

        if not self.metadata.md5:
            out.error('no testing template loaded')
            return

        if args:
            targets = enabled_targets(self.targets)

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, args.split(','))

            if targets:
                patchinfo = '/'.join([config.patchinfo_url, self.metadata.md5])
                destination = '/'.join([config.target_tempdir, self.metadata.md5])
                fetchcmd = 'mkdir -p %s; cd %s; wget -q -r -nd -l2 --no-parent -A "*.src.rpm" %s/' \
                    % (destination, destination, lelf.metadata.md5)
                installcmd = 'cd %s; rpm -Uhv *.src.rpm' % destination

                RunCommand(targets, fetchcmd).run()
                RunCommand(targets, installcmd).run()

                out.info('done')
        else:
            self.parse_error(self.do_source_install, args)

    def complete_source_install(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    def do_source_extract(self, args):
        """
        Extracts current source RPMs to a local temporary directory.
        If no filename is given, the whole package content is extracted.

        source_extract [filename]
        Keyword arguments:
        filename -- filename to extract
        """

        if not self.metadata.md5:
            out.error('no testing template loaded')
            return

        destination = os.path.join(config.local_tempdir, self.metadata.md5)
        pattern = ''

        if args:
            pattern = args

        try:
            os.makedirs(destination)
        except OSError, error:
            if error.errno == errno.EEXIST:
                pass
        except Exception, error:
            out.critical('failed to create temp directory: %s' % str(error))
            return

        patchinfo = '/'.join([config.patchinfo_url, self.metadata.md5])

        exitcode = os.system('cd %s; wget -q -r -nd -l2 --no-parent -A "*src.rpm" %s/'
                             % (destination, patchinfo))
        if exitcode:
            out.error('failed to fetch src rpm')
            return
        exitcode = \
            os.system('cd %s; for i in *src.rpm; do name=$(rpm -qp --queryformat "%%{NAME}" $i); mkdir -p $name; cd $name; rpm2cpio ../$i | cpio -i --unconditional --preserve-modification-time --make-directories %s; cd ..; done'
                       % (destination, pattern))
        if exitcode:
            out.error('failed to extract src rpm')
            return

        out.info('src rpm was extracted to %s' % destination)

    def do_source_diff(self, args):
        """
        Creates a source diff between the update package and the currently
        installed package. If the diff needs to be against the latest
        released package, make sure to run "prepare" first.

        If diff type "source" is set, a package source diff is created.
        This creates usually a diff of the specfile and new patchfiles.

        If diff type "build" is set, a build diff is created.
        This creates a diff between the patched build directories and
        is usually architecture dependend.

        The osc command line client needs to be installed first.

        source_diff <type>
        Keyword arguments:
        type     -- "build" or "source" diff
        """

        if not self.metadata.md5:
            out.error('no testing template loaded')
            return

        if args in ['source', 'build']:
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

            targets = enabled_targets(self.targets)

            updated = {}
            installed = {}
            destination = os.path.join(config.local_tempdir, self.metadata.md5)

            if not glob.glob(os.path.join(destination, '*', '*.spec')):
                self.do_source_extract('')

            for rpmfile in glob.glob(os.path.join(destination, '*.src.rpm')):
                try:
                    match = re.search('obs://.*/(.*)/.*/(\w+)-(.*)', RPMFile(rpmfile).disturl)
                except Exception, error:
                    out.critical('failed to open %s: %s' % (rpmfile, error))
                    if unicode(error) == u'public key not available':
                        out.critical('Public key is not available.')
                        out.critical('In order to import new keys, you should run the following command as root:')
                        out.critical('cd /tmp; wget -q -r -nd -l1 --no-parent -A "*.asc" http://download.suse.de/keys/; for i in *.asc; do rpm --import $i; done')
                    continue

                if match:
                    disturl = match.group(0)
                    project = match.group(1)
                    commit = match.group(2)
                    name = match.group(3)
                    updated[name] = {'project': project, 'commit': commit, 'disturl': disturl}

            # if there are src.rpm package names which are not reflected by
            # binary rpms, check all binary rpms for this specific
            # src.rpm/disturl name
            if filter(lambda x: x not in self.metadata.get_package_list(), updated.keys()):
                search_list = self.metadata.get_package_list()
            else:
                search_list = updated.keys()

            for package in search_list:
                RunCommand(targets, 'rpm -q --qf "%%{DISTURL}" %s' % package).run()

                for target in targets:
                    line = targets[target].lastout().split('\n')[0]
                    match = re.search('obs://.*/(.*)/.*/(\w+)-(.*)', line)
                    if match:
                        disturl = match.group(0)
                        project = match.group(1)
                        commit = match.group(2)
                        name = match.group(3)
                        installed[name] = {'project': project, 'commit': commit, 'disturl': disturl}

            for name in updated.keys():
                try:
                    assert(installed[name] and updated[name])
                except (AssertionError, KeyError):
                    out.warning('osc disturl not found for package %s. skipping.' % name)
                    continue

                if installed[name]['commit'] == updated[name]['commit']:
                    out.warning('revision of package %s hasn\'t changed, it\'s most likely aready updated. skipping.' % name)
                    continue

                diff = os.path.join(destination, '%s-%s.diff' % (name, args))
                if args == 'source':
                    with open(diff, 'w+') as f:
                        try:
                            f.write(osc.core.server_diff('https://api.suse.de', installed[name]['project'], name,
                                installed[name]['commit'], updated[name]['project'], name, updated[name]['commit'], unified=True))
                        except Exception, error:
                            out.error('failed to diff packages: %s', error)
                            return

                elif args == 'build':
                    RunCommand(targets, 'which osc').run()
                    for target in targets:
                        if targets[target].lastexit() != 0:
                            out.error('osc is missing on %s. skipping.' % target)

                    for state in ['new', 'old']:
                        sourcedir = os.path.join(destination, name, state)
                        builddir = os.path.join(destination, name, state, 'BUILD')
                        if state == 'new':
                            disturl = updated[name]['disturl']
                        else:
                            disturl = installed[name]['disturl']

                        RunCommand(targets, 'echo "[general]\n[https://api.suse.de]\nuser = qa\npass = qa" >/tmp/osc.mtui').run()
                        RunCommand(targets, 'mkdir -p %s' % builddir).run()
                        RunCommand(targets, 'cd %s; osc -c /tmp/osc.mtui -q -A "https://api.suse.de" co -c %s' % (sourcedir, disturl)).run()
                        RunCommand(targets, 'rpmbuild --quiet --nodeps --define "_sourcedir %s/%s" --define "_builddir %s" -bp %s/%s/*.spec'
                                % (sourcedir, name, builddir, sourcedir, name)).run()

                    RunCommand(targets, 'diff -x ".osc" -Naur %s/../old/BUILD %s/../new/BUILD > %s' % (sourcedir, sourcedir, diff)).run()

                if args == 'source':
                    out.info('wrote diff locally to %s' % diff)
                elif args == 'build':
                    out.info('wrote diff remotely to %s' % diff)

        else:
            self.parse_error(self.do_source_diff, args)

    def complete_source_diff(self, text, line, begidx, endidx):
        return [i for i in ['source', 'build'] if i.startswith(text)]

    def do_source_verify(self, args):
        """
        Verifies SPECFILE content. Makes sure that every Patch entry
        is applied.

        source_verify
        Keyword arguments:
        None
        """

        if not self.metadata.md5:
            out.error('no testing template loaded')
            return

        if args:
            self.parse_error(self.do_source_verify, args)

        destination = os.path.join(config.local_tempdir, self.metadata.md5)

        specfiles = glob.glob(os.path.join(destination, '*', '*.spec'))

        if not specfiles:
            self.do_source_extract('*.spec')
            specfiles = glob.glob(os.path.join(destination, '*', '*.spec'))
            if not specfiles:
                out.error('failed to load specfile')
                return

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

            if not patches:
                out.warning('no patch entries found in specfile')
                return

            print 'Patches in %s:' % specfile
            for patch in patches:
                num = filter(str.isdigit, patch) or 0
                if num == 0 and re.findall('\'%patch\W+', str(content)):
                    result = green('applied')
                elif re.findall('\'%%%s%s\W+' % ('patch', num), str(content)):
                    result = green('applied')
                elif re.findall('patch.*%%{P:%s}' % num, str(content)):
                    result = green('applied')
                else:
                    result = red('not applied')

                print '{0:45}: {1}'.format(patches[patch].replace('name}', name), result)

    def do_list_packages(self, args):
        """
        Lists current installed package versions from the targets if a
        target is specified. If none is specified, all required package
        versions which should be installed after the update are listed.
        If version 0 is shown for a package, the package is not installed.

        list_packages [hostname]
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        """

        if args:
            targets = enabled_targets(self.targets)

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, args.split(','))

            for host in sorted(targets.values()):
                host.query_versions()
                print 'packages on %s (%s):' % (host.hostname, host.system)
                for package in host.packages:
                    current = host.packages[package].current
                    required = self.metadata.packages[package]
                    if current == '0':
                        state = blue('not installed')
                    elif RPMVersion(current) > RPMVersion(required):
                        state = red('too recent')
                    elif RPMVersion(current) < RPMVersion(required):
                        state = yellow('update needed')
                    else:
                        state = green('updated')

                    print '{0:30}: {1:15} {2}'.format(package, host.packages[package].current, state)

                print
        else:
            for (package, version) in self.metadata.packages.items():
                print '{0:30}: {1}'.format(package, version)

    def complete_list_packages(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    def do_add_scripts(self, args):
        """
        Add check script to the pre/post testruns

        add_scripts <script>[,script,...]
        Keyword arguments:
        script   -- script name to add to the testrun
        """

        if not self.metadata.md5:
            out.error('no testing template loaded')
            return

        if args:
            for script in args.split(','):
                src = os.path.join(self.datadir, 'helper', script)
                destdir = os.path.join(os.path.dirname(self.metadata.path), 'scripts')

                try:
                    for state in ['pre', 'post']:
                        dest = os.path.join(destdir, state, script)
                        shutil.copy(src, dest)

                    src = os.path.join(self.datadir, 'helper', script.replace('check_', 'compare_'))
                    dest = os.path.join(destdir, 'compare', script.replace('check_', 'compare_'))
                    try:
                        shutil.copy(src, dest)
                    except IOError:
                        # ignore missing compare scripts
                        pass

                except IOError:
                    out.error('failed to copy script %s' % script)
                else:
                    out.info('done')

        else:
            self.parse_error(self.do_add_scripts, args)

    def complete_add_scripts(self, text, line, begidx, endidx):
        scripts = os.listdir(os.path.join(self.datadir, 'helper'))
        return [script for script in scripts if script.startswith(text) and 'check' in script and script not in line]

    def do_remove_scripts(self, args):
        """
        Remove check script from the pre/post testruns

        add_scripts <script>[,script,...]
        Keyword arguments:
        script   -- script name to remove from the testrun
        """

        if not self.metadata.md5:
            out.error('no testing template loaded')
            return

        if args:
            for script in args.split(','):
                directory = os.path.join(os.path.dirname(self.metadata.path), 'scripts')

                try:
                    for state in ['pre', 'post']:
                        os.remove(os.path.join(directory, state, script))

                    compare = os.path.join(directory, 'compare', script.replace('check_', 'compare_'))
                    os.remove(compare)

                except (IOError, OSError):
                    out.debug('failed to remove script %s' % script)
                else:
                    out.info('done')

        else:
            self.parse_error(self.do_remove_scripts, args)

    def complete_remove_scripts(self, text, line, begidx, endidx):
        pre = os.listdir(os.path.join(os.path.dirname(self.metadata.path), 'scripts', 'pre'))
        post = os.listdir(os.path.join(os.path.dirname(self.metadata.path), 'scripts', 'post'))
        return [script for script in set(pre) & set(post) if script.startswith(text) and 'check' in script and script not in line]

    def do_list_scripts(self, args):
        """
        List available scripts from the scripts subdirectory. This scripts
        are run in a pre updated state and in the post updated state.

        list_scripts
        Keyword arguments:
        None
        """

        if not self.metadata.md5:
            out.error('no testing template loaded')
            return

        if args:
            self.parse_error(self.do_list_scripts, args)
        else:

            for (root, dirs, files) in os.walk(os.path.join(os.path.dirname(self.metadata.path), 'scripts')):
                for name in files:
                    if not '.svn' in root:
                        print os.path.join(root, name)

    def do_list_update_commands(self, args):
        """
        List all commands which are invoked when applying updates on the
        target hosts.

        list_update_commands
        Keyword arguments:
        None
        """

        if not self.metadata.md5:
            out.error('no testing template loaded')
            return

        if args:
            self.parse_error(self.do_list_update_commands, args)
        else:

            release = self.metadata.get_release()

            try:
                updater = Updater[release]
            except KeyError:
                out.critical('no updater available for %s' % release)
                return

            print '\n'.join(updater(self.targets, self.metadata.patches, self.metadata.get_package_list()).commands)
            del updater

    def do_list_downgrade_commands(self, args):
        """
        List all commands which are invoked when downgrading packages on the
        target hosts.

        list_downgrade_commands
        Keyword arguments:
        None
        """

        if not self.metadata.md5:
            out.error('no testing template loaded')
            return

        if args:
            self.parse_error(self.do_list_update_commands, args)
        else:

            release = self.metadata.get_release()

            try:
                downgrader = Downgrader[release]
            except KeyError:
                out.critical('no downgrader available for %s' % release)
                return

            print '\n'.join(downgrader(self.targets, self.metadata.get_package_list(), self.metadata.patches).commands)
            del downgrader

    def do_testopia_list(self, args):
        """
        List all Testopia package testcases for the current product.
        If now packages are set, testcases are displayed for the
        current update.

        testopia_list [package,package,...]
        Keyword arguments:
        package  -- packag to display testcases for
        """

        try:
            assert(self.testopia.testcases and not args)
        except (AttributeError, AssertionError):
            release = self.metadata.get_release()
            packages = args.split(',')
            if not packages[0]:
                packages = self.metadata.get_package_list()

            self.testopia = Testopia(release, packages)

        url = config.bugzilla_url

        if not self.testopia.testcases:
            out.info('no testcases found')

        for case_id in self.testopia.testcases:
            summary = self.testopia.testcases[case_id]['summary']
            if self.testopia.testcases[case_id]['status'] == 'disabled':
                status = red('disabled')
            elif self.testopia.testcases[case_id]['status'] == 'confirmed':
                status = green('confirmed')
            else:
                status = yellow('proposed')
            if self.testopia.testcases[case_id]['automated'] == 'yes':
                automated = 'automated'
            else:
                automated = 'manual'
            print '{0:40}: {1} ({2})'.format(summary, status, automated)
            print '%s/tr_show_case.cgi?case_id=%s' % (url, case_id)
            print

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

            try:
                assert(self.testopia)
            except (AttributeError, AssertionError):
                release = self.metadata.get_release()
                packages = self.metadata.get_package_list()

                self.testopia = Testopia(release, packages)

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
                    print blue('Testcase summary:'), testcase['summary']
                    print blue('Testcase URL:'), '%s/tr_show_case.cgi?case_id=%s' % (url, case_id)
                    print blue('Testcase automated:'), testcase['automated']
                    print blue('Testcase status:'), testcase['status']
                    print blue('Testcase requirements:'), testcase['requirement']
                    if testcase['setup']:
                        print blue('Testcase setup:')
                        print testcase['setup']
                    if testcase['breakdown']:
                        print blue('Testcase breakdown:')
                        print testcase['breakdown']
                    print blue('Testcase actions:')
                    print testcase['action']
                    if testcase['effect']:
                        print blue('Testcase effect:')
                        print testcase['effect']

        else:
            self.parse_error(self.do_testopia_show, args)

    def complete_testopia_show(self, text, line, begidx, endidx):
        if not line.count(','):
            return self.complete_testopia_testcaselist(text, line, begidx, endidx)

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

            try:
                assert(self.testopia)
            except (AttributeError, AssertionError):
                release = self.metadata.get_release()
                packages = self.metadata.get_package_list()

                self.testopia = Testopia(release, packages)

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

            try:
                assert(self.testopia)
            except (AttributeError, AssertionError):
                release = self.metadata.get_release()
                packages = self.metadata.get_package_list()

                self.testopia = Testopia(release, packages)

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

    def do_list_testsuite_commands(self, args):
        """
        List all commands which are invoked when running ctcs2 testsuites
        on the target hosts.

        list_testsuite_commands
        Keyword arguments:
        None
        """

        if args:
            self.parse_error(self.do_list_testsuite_commands, args)
        else:

            time = date.today().strftime('%d/%m/%y')
            swampid = self.metadata.swampid
            username = config.session_user

            comment = 'testing <testsuite> on SWAMP %s on %s' % (swampid, time)

            print 'export TESTS_LOGDIR=/var/log/qa/%s; <testsuite>' % self.metadata.md5
            print '/usr/share/qa/tools/remote_qa_db_report.pl -b -t patch:%s -T %s -f /var/log/qa/%s -c \'%s\'' % (self.metadata.md5,
                    username, self.metadata.md5, comment)

    def do_list_bugs(self, args):
        """
        Lists related bugs and corresponding Bugzilla URLs.

        list_bugs
        Keyword arguments:
        None
        """

        if args:
            self.parse_error(self.do_list_bugs, args)
        else:

            buglist = ','.join(sorted(self.metadata.bugs.keys()))

            url = config.bugzilla_url

            print 'Buglist: %s/buglist.cgi?bug_id=%s' % (url, buglist)
            for (bug, description) in self.metadata.bugs.items():
                print
                print 'Bug #{0:5}: {1}'.format(bug, description)
                print '%s/show_bug.cgi?id=%s' % (url, bug)

    def do_list_metadata(self, args):
        """
        Lists patchinfo metadata like patch number, SWAMP ID or packager.

        list_metadata
        Keyword arguments:
        None
        """

        if args:
            self.parse_error(self.do_list_metadata, args)
        else:

            targetlist = ' '.join(sorted(self.targets.keys()))
            packagelist = ' '.join(sorted(self.metadata.get_package_list()))
            if self.metadata.md5:
                patchinfo = '/'.join([config.patchinfo_url, self.metadata.md5])
                report = '/'.join([config.reports_url, self.metadata.md5, 'log'])

            print '{0:15}: {1}'.format('MD5SUM', self.metadata.md5)
            print '{0:15}: {1}'.format('SWAMP ID', self.metadata.swampid)
            print '{0:15}: {1}'.format('Category', self.metadata.category)
            print '{0:15}: {1}'.format('Reviewer', self.metadata.reviewer)
            print '{0:15}: {1}'.format('Packager', self.metadata.packager)
            for (type, id) in self.metadata.patches.items():
                print '{0:15}: {1}'.format(type.upper(), id)
            print '{0:15}: {1}'.format('Bugs', ', '.join(self.metadata.bugs.keys()))
            print '{0:15}: {1}'.format('Hosts', targetlist)
            print '{0:15}: {1}'.format('Packages', packagelist)
            try:
                print '{0:15}: {1}'.format('Build', patchinfo)
                print '{0:15}: {1}'.format('Testreport', report)
            except UnboundLocalError:
                pass

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

        if args:
            packages = args.replace(',', ' ')
        else:
            packages = ' '.join(self.metadata.get_package_list())

        targets = enabled_targets(self.targets)

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
                    print 'version history from:'
                    for target in history[path]:
                        print '  %s (%s)' % (target, targets[target].system)
                print

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
                    print '%s:' % package
                    indent = 0
                    for version in sorted(release[package], key=RPMVersion, reverse=True):
                        print '  ' * indent + '-> %s' % version
                        indent = indent + 1
                    print

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
            targets = enabled_targets(self.targets)

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, args.split(','))

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
            targets = selected_targets(self.targets, [args])

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

        if args:
            targets = enabled_targets(self.targets)

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, set(targets) & set(args.split(',')))

            command = ''.join(set(args.split(',')) - set(self.targets) - set(['all']))

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
        else:
            self.parse_error(self.do_run, args)

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

            targets = enabled_targets(self.targets)

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, args.split(','))

            for target in targets:
                print 'testsuites on %s (%s):' % (target, targets[target].system)
                print '\n'.join([i for i in sorted(targets[target].listdir(path)) if i.endswith('-run')])
                print
        else:
            self.parse_error(self.do_testsuite_list, args)

    def complete_testsuite_list(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

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

        (args, _, command) = args.rpartition(',')

        if args and command:
            targets = enabled_targets(self.targets)

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, args.split(','))

            if not command.startswith('/'):
                command = os.path.join(config.target_testsuitedir, command.strip())

            command = 'export TESTS_LOGDIR=/var/log/qa/%s; %s' % (self.metadata.md5, command)
            name = os.path.basename(command).replace('-run', '')

            if targets:
                try:
                    RunCommand(targets, command).run()
                except KeyboardInterrupt:
                    out.info('testsuite run canceled')
                    return

                for target in targets:
                    print '%s:~> %s-testsuite [%s]' % (target, name, targets[target].lastexit())
                    print targets[target].lastout()
                    if targets[target].lasterr():
                        print targets[target].lasterr()

                out.info('done')
        else:

            self.parse_error(self.do_testsuite_run, args)

    def complete_testsuite_run(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

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

        (args, _, command) = args.rpartition(',')

        if args and command:
            targets = enabled_targets(self.targets)

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, args.split(','))

            name = os.path.basename(command).replace('-run', '')
            time = date.today().strftime('%d/%m/%y')
            swampid = self.metadata.swampid
            username = config.session_user

            comment = 'testing %s (SWAMP %s) on %s' % (name, swampid, time)

            comment = edit_text(comment)

            if len(comment) > 100:
                out.warning('comment strings > 100 chars are truncated by remote_qa_db_report.pl')

            out.info('please specify rd-qa NIS password')
            password = getpass.getpass()

            submit = []
            submit.append('echo \'echo -n "%s"\' > /tmp/pwdask' % password)
            submit.append('chmod 700 /tmp/pwdask')
            submit.append('SSH_ASKPASS=/tmp/pwdask DISPLAY=dummydisplay:0 /usr/share/qa/tools/remote_qa_db_report.pl -b -t patch:%s -T %s -f /var/log/qa/%s -c \'%s\''
                           % (self.metadata.md5, username, self.metadata.md5, comment))
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
                            print '%s:~> %s [%s]' % (target, name, targets[target].lastexit())
                            print targets[target].lastout()
                            if targets[target].lasterr():
                                print targets[target].lasterr()
                        else:
                            match = re.search('(http://.*/submission.php.submission_id=\d+)', targets[target].lasterr())
                            if match:
                                system = targets[target].system
                                out.info('submission for %s (%s): %s' % (target, system, match.group(1)))
                            else:
                                out.critical('no submission found for %s. please use "show_log %s" to see what went wrong' % (target,
                                             target))

            out.info('done')
        else:
            self.parse_error(self.do_testsuite_submit, args)

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

        if args:
            self.session = args
            self.prompt = 'QA:%s > ' % self.session
        else:
            self.session = self.metadata.md5
            self.prompt = 'QA > '

    def do_load_template(self, args):
        """
        Load QA Maintenance template by md5 identifier. All changes and logs
        from an already loaded template are lost if not saved previously.
        Already connected hosts are kept and extended by the reference hosts
        defined in the template file.

        load_template <md5>
        Keyword arguments:
        md5      -- md5 update identifier
        """

        # check if args is a valid md5 hash
        match = re.match(r'([a-fA-F\d]{32})', args)
        try:
            md5 = match.group(1)
        except AttributeError:
            out.error('md5 hash "%s" not valid' % args)
            self.parse_error(self.do_load_template, args)
            return

        if self.metadata.md5:
            if not input('should i overwrite already loaded session %s? (y/N) ' % self.metadata.md5, ['y', 'yes'], self.interactive):
                return

        try:
            testreport = TestReportFactory(self.config, self.log, md5)
        except Exception:
            print_exc()
            return

        # Reload hosts to which we already have a connection
        # close hosts we are already connected to but add them to the
        # testreport.systems so they get connected to again.
        # This feature comes from pre-1.0 versions.
        # NOTE: the only reason we need to reconnect seems to be that
        # when the L{Target} object is created, it is passed a list of
        # packages, which changes with the testreport change. So this
        # may go away when refactored.
        for hostname, target in self.targets.items():
            target.close()
            testreport.add_host(hostname, target.system)

        testreport.load_systems_from_testplatforms()
        self.targets = testreport.connect_targets()
        self.metadata = testreport

    def do_set_location(self, args):
        """
        Change current reference host location to another site.

        set_location <site>
        Keyword arguments:
        site     -- location name
        """

        if args:
            out.info('changed location from "%s" to "%s"' % (self.metadata.location, args))
            self.metadata.location = args
        else:
            self.parse_error(self.do_set_location, args)

    def complete_set_location(self, text, line, begidx, endidx):
        refhost = Refhost(config.refhosts_xml)
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

        (args, _, state) = args.rpartition(',')

        if args and state:
            targets = enabled_targets(self.targets)

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, args.split(','))

            if state == 'enabled':
                comment = raw_input('comment: ').strip()

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

        (args, _, state) = args.rpartition(',')

        if args and state:
            targets = self.targets

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, args.split(','))

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

        (args, _, timeout) = args.rpartition(',')

        if args and timeout:
            targets = self.targets

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, args.split(','))

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

        (args, _, name) = args.rpartition(',')

        if args and name:
            targets = enabled_targets(self.targets)

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, args.split(','))

            for target in targets:
                lock = targets[target].locked()
                if lock.locked and not lock.own():
                    out.warning('host %s is locked since %s by %s. skipping.' % (target, lock.time(), lock.user))
                    if lock.comment:
                        out.info("%s's comment: %s" % (lock.user, lock.comment))
                else:
                    try:
                        targets[target].set_repo(name.upper())
                    except ValueError:
                        self.parse_error(self.do_set_repo, args)
        else:

            self.parse_error(self.do_set_repo, args)

    def complete_set_repo(self, text, line, begidx, endidx):
        if line.count(','):
            return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx, ['testing', 'update'])
        else:
            return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    def do_install(self, args):
        """
        Installs packages from the current active repository.
        The repository should be set with the set_repo command beforehand.

        install <hostname>[,hostname,...],<package>[ package ...]
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        package  -- package name
        """

        (args, _, packages) = args.rpartition(',')

        if args and packages:
            targets = enabled_targets(self.targets)

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, args.split(','))

            if targets:
                release = self.metadata.get_release()
                try:
                    installer = Installer[release]
                except KeyError:
                    out.critical('no installer available for %s' % release)
                    return

                out.info('installing')
                for target in targets:
                    targets[target].add_history(['install', packages])

                try:
                    installer(targets, packages.split()).run()
                except Exception:
                    out.critical('failed to install packages')
                    return
                except KeyboardInterrupt:
                    out.info('installation process canceled')
                    return
                else:
                    out.info('done')
        else:

            self.parse_error(self.do_install, args)

    def complete_install(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    def do_uninstall(self, args):
        """
        Removes packages from the system.

        uninstall <hostname>[,hostname,...],<package>[ package ...]
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        package  -- package name
        """

        (args, _, packages) = args.rpartition(',')

        if args and packages:
            targets = enabled_targets(self.targets)

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, args.split(','))

            if targets:
                release = self.metadata.get_release()
                try:
                    uninstaller = Uninstaller[release]
                except KeyError:
                    out.critical('no uninstaller available for %s' % release)
                    return

                out.info('removing')
                try:
                    uninstaller(targets, packages.split()).run()
                except Exception:
                    out.critical('failed to remove packages')
                    return
                except KeyboardInterrupt:
                    out.info('uninstallation process canceled')
                    return
                else:
                    out.info('done')
        else:

            self.parse_error(self.do_uninstall, args)

    def complete_uninstall(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    def do_downgrade(self, args):
        """
        Downgrades all related packages to the last released version (using
        the UPDATE channel). This does not work for SLES 9 hosts, though.

        downgrade <hostname>
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        """

        if not self.metadata.md5:
            out.error('no testing template loaded')
            return

        if args:
            targets = enabled_targets(self.targets)

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, args.split(','))

            if targets:
                release = self.metadata.get_release()

                try:
                    downgrader = Downgrader[release]
                except KeyError:
                    out.critical('no downgrader available for %s' % release)
                    return

                out.info('downgrading')
                for target in targets:
                    targets[target].add_history(['downgrade', self.metadata.md5, ' '.join(self.metadata.get_package_list())])

                try:
                    downgrader(targets, self.metadata.get_package_list(), self.metadata.patches).run()
                except Exception:
                    out.critical('failed to downgrade target systems')
                    return
                except KeyboardInterrupt:
                    out.info('downgrade process canceled')
                    return
                else:
                    out.info('done')
        else:

            self.parse_error(self.do_downgrade, args)

    def complete_downgrade(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

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

        if not self.metadata.md5:
            out.error('no testing template loaded')
            return

        if args:
            force = False
            installed = False
            testing = False

            parameter = args.split(',')
            if 'force' in parameter:
                force = True
                parameter.remove('force')
            if 'installed' in parameter:
                installed = True
                parameter.remove('installed')
            if 'testing' in parameter:
                testing = True
                parameter.remove('testing')

            args = ','.join(parameter)
            targets = enabled_targets(self.targets)

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, args.split(','))

            if targets:
                release = self.metadata.get_release()

                try:
                    preparer = Preparer[release]
                except KeyError:
                    out.critical('no preparer available for %s' % release)
                    return True

                out.info('preparing')
                try:
                    preparer(targets, self.metadata.get_package_list(), force=force, installed_only=installed, testing=testing).run()
                except Exception:
                    out.critical('failed to prepare target systems')
                    return False
                except KeyboardInterrupt:
                    out.info('preparation process canceled')
                    return False
                else:
                    out.info('done')
        else:

            self.parse_error(self.do_prepare, args)

    def complete_prepare(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx, ['force', 'installed', 'testing'])

    def do_update(self, args):
        """
        Applies the testing update to the target hosts. While updating the
        machines, the pre-, post- and compare scripts are run before and
        after the update process. If the update adds new packages to the
        channel, the "newpackage" parameter triggers the package installation
        right after the update. To skip the preparation procedure, append
        "noprepare" to the argument list.

        update <hostname>[,newpackage][,noprepare]
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        """

        if not self.metadata.md5:
            out.error('no testing template loaded')
            return

        if args:
            prepare = True
            missing = False
            newpackage = False

            parameter = args.split(',')

            # don't install new packages when doing a non-interactive kernel update
            if not self.interactive and filter(lambda x: x in ['-kmp-', 'kernel-default'], self.metadata.packages):
                try:
                    parameter.remove('newpackage')
                except ValueError:
                    pass
                parameter.append('installed')

            if 'newpackage' in parameter:
                newpackage = True
                parameter.remove('newpackage')

            if 'noprepare' in parameter:
                prepare = False
                parameter.remove('noprepare')

            args = ','.join(parameter)
            targets = enabled_targets(self.targets)

            if prepare:
                if self.do_prepare(args) is False:
                    return

            if args.split(',')[0] != 'all':
                targets = selected_targets(targets, args.split(','))

            for target in targets:
                lock = targets[target].locked()
                if lock.locked and not lock.own():
                    out.warning('host %s is locked since %s by %s. aborting.' % (target, lock.time(), lock.user))
                    if lock.comment:
                        out.info("%s's comment: %s" % (lock.user, lock.comment))
                    return

            for target in targets:
                targets[target].set_locked()
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

            if missing and input('there were missing packages. cancel update process? (y/N) ', ['y', 'yes'], self.interactive):
                for target in targets:
                    if not lock.locked:
                        targets[target].remove_lock()
                return

            script_hook(targets, 'pre', os.path.dirname(self.metadata.path), self.metadata.md5)

            out.info('updating')

            release = self.metadata.get_release()
            try:
                updater = Updater[release]
            except KeyError:
                out.critical('no updater available for %s' % release)
                for target in targets:
                    if not lock.locked:
                        targets[target].remove_lock()
                return

            out.debug("chosen updater: %s" % repr(updater))

            try:
                updater(targets, self.metadata.patches, self.metadata.get_package_list()).run()
            except Exception:
                out.critical('failed to update target systems')
                for target in targets:
                    if not lock.locked:
                        targets[target].remove_lock()
                Notification('MTUI', 'updating %s failed' % self.session, 'stock_dialog-error').show()
                return
            except KeyboardInterrupt:
                out.info('update process canceled')
                for target in targets:
                    if not lock.locked:
                        targets[target].remove_lock()
                return

            if newpackage:
                self.do_prepare('%s,testing' % args)

            missing = False
            for target in targets:
                targets[target].add_history(['update', self.metadata.md5, ' '.join(self.metadata.get_package_list())])
                packages = targets[target].packages

                targets[target].query_versions()

                for package in packages:
                    before = packages[package].before
                    required = packages[package].required
                    after = targets[target].packages[package].current

                    packages[package].set_versions(after=after)

                    if after is not None and after != '0':
                        if RPMVersion(before) == RPMVersion(after):
                            missing = True
                            out.warning('%s: package was not updated: %s (%s)' % (target, package, after))

                        if RPMVersion(after) < RPMVersion(required):
                            missing = True
                            out.warning('%s: package does not match required version: %s (%s, required %s)' % (target, package, after,
                                        required))

            if missing and input("some packages haven't been updated. cancel update process? (y/N) ", ['y', 'yes'], self.interactive):
                for target in targets:
                    if not lock.locked:
                        targets[target].remove_lock()
                return

            script_hook(targets, 'post', os.path.dirname(self.metadata.path), self.metadata.md5)
            script_hook(targets, 'compare', os.path.dirname(self.metadata.path), self.metadata.md5)
            FileDelete(targets, os.path.join(config.target_tempdir, self.metadata.md5, 'output')).run()

            for target in targets:
                if not lock.locked:
                    targets[target].remove_lock()

            Notification('MTUI', 'updating %s finished' % self.session).show()
            out.info('done')
        else:
            self.parse_error(self.do_update, args)

    def complete_update(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx, ['newpackage', 'noprepare'])

    def do_list_sessions(self, args):
        """
        Lists current active ssh sessions on target hosts.

        list_sessions <hostname>
        Keyword arguments:
        hostname -- hostname from the target list or "all"
        """

        if not args:
            args = 'all'

        command = "ss -r  | sed -n 's/^[^:]*:ssh *\([^ ]*\):.*/\\1/p' | sort -u"

        targets = enabled_targets(self.targets)

        if args.split(',')[0] != 'all':
            targets = selected_targets(targets, args.split(','))

        if targets:
            try:
                RunCommand(targets, command).run()
            except KeyboardInterrupt:
                return

        for host in sorted(targets.values()):
            print 'sessions on %s (%s):' % (host.hostname, host.system)
            print host.lastout()

    def complete_list_sessions(self, text, line, begidx, endidx):
        return self.complete_enabled_hostlist_with_all(text, line, begidx, endidx)

    def do_checkout(self, args):
        """
        Update template files from the SVN.

        checkout
        Keyword arguments:
        none
        """

        if not self.metadata.md5:
            out.error('no testing template loaded')
            return

        exitcode = os.system('cd %s; svn up' % os.path.dirname(self.metadata.path))

        if exitcode != 0:
            out.error('updating template failed, returncode: %s' % exitcode)

    def do_commit(self, args):
        """
        Commits the testing template to the SVN. This can be run after the
        testing has finished an the template is in the final state.

        commit [message]
        Keyword arguments:
        message  -- commit message
        """

        if not self.metadata.md5:
            out.error('no testing template loaded')
            return

        message = ''
        if args:
            message = '-m "%s"' % args

        exitcode = os.system('cd %s; svn up; svn ci %s' % (os.path.dirname(self.metadata.path), message))

        if exitcode != 0:
            out.error('committing template failed, returncode: %s' % exitcode)

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

        if args:
            targets = self.targets

            for filename in glob.glob(args):
                if os.path.isfile(filename):
                    remote = os.path.join(config.target_tempdir, self.metadata.md5, os.path.basename(filename))

                    FileUpload(targets, filename, remote).run()
                    out.info('uploaded %s to %s' % (filename, remote))
        else:

            self.parse_error(self.do_put, args)

    def complete_put(self, text, line, begidx, endidx):
        return self.complete_filelist(text, line, begidx, endidx)

    def do_get(self, args):
        """
        Downloads a file from all enabled hosts. Multiple files can not be
        selected. Files are saved in the $templatedir/downloads/ subdirectory
        with the hostname as file extension.

        get <remote filename>
        Keyword arguments:
        filename -- file to download from the target hosts
        """

        if args:
            targets = self.targets

            destination = os.path.join(os.path.dirname(self.metadata.path), 'downloads')
            local = os.path.join(destination, os.path.basename(args))

            try:
                os.makedirs(destination)
            except OSError, error:
                if error.errno == errno.EEXIST:
                    pass
            except Exception, error:
                out.critical('failed to create directories: %s' % str(error))
                return

            FileDownload(targets, args, local, True).run()
            out.info('downloaded %s to %s' % (args, local))
        else:

            self.parse_error(self.do_get, args)

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
                    os.system('%s %s' % (path, ' '.join(hosts)))
                except Exception:
                    out.error('running %s failed' % filename)
            else:
                out.error('%s script not found, make sure term.%s.sh exists' % (args, args))
                self.parse_error(self.do_terms, args)
        else:

            print 'available terminals scripts:'
            for filename in glob.glob(os.path.join(dirname, 'term.*.sh')):
                print os.path.basename(filename).split('.')[1]

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
        if not self.metadata.md5 and command != 'file':
            out.error('no testing template loaded')
            return

        if command == 'file':
            path = filename
        elif command == 'template':
            path = self.metadata.path
        elif command == 'specfile':
            path = os.path.join(config.local_tempdir, self.metadata.md5, '*', '*.spec')
            if not glob.glob(path):
                self.do_source_extract(None)
        elif command == 'patch':
            path = os.path.join(config.local_tempdir, self.metadata.md5, '*', filename)
            if not glob.glob(path):
                self.do_source_extract(None)
        else:
            self.parse_error(self.do_edit, args)
            return

        os.system('%s %s' % (editor, path))

    def complete_edit(self, text, line, begidx, endidx):
        if 'file,' in line:
            return self.complete_filelist(text.replace('file,', '', 1), line, begidx, endidx)
        if 'patch,' in line:
            specfile = glob.glob(os.path.join(config.local_tempdir, self.metadata.md5, '*', '*.spec'))
            with open(specfile, 'r') as spec:
                name = re.findall('Name:\W+(.*)', spec.read())[0]
                spec.seek(0)
                return [i for i in [s.replace('name}', name) for s in re.findall('Patch\d*:\W+(.*)', spec.read())] if i.startswith(text)]
        else:
            return [i for i in ['file,', 'template', 'specfile', 'patch,'] if i.startswith(text)]

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

        if not self.metadata.md5:
            out.error('no testing template loaded')
            return

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
            if not input('should i overwrite %s? (y/N) ' % filename, ['y', 'yes'], self.interactive):
                filename += '.' + timestamp()

        out.info('exporting XML to %s' % filename)
        try:
            with open(filename, 'w') as f:
                f.write('\n'.join(l.rstrip().encode('utf-8') for l in template))
        except IOError, error:
            print 'failed to write %s: %s' % (filename, error.strerror)
        else:
            print 'wrote template to %s' % filename

    def complete_export(self, text, line, begidx, endidx):
        return self.complete_hostlist(text, line, begidx, endidx, ['force'])

    def do_save(self, args):
        """
        Save the testing log to a XML file. All commands and package
        versions are saved there. When no parameter is given, the XML is saved
        to $templatedir/output/log.xml. If that file already exists and the
        tester doesn't want to overwrite it, a postfix (current timestamp)
        is added to the filename. The log can be used to fill the required
        sections of the testing template after the testing has finished.
        This could be done with the convert.py script.

        save [filename]
        Keyword arguments:
        filename -- save log as file filename
        """

        targets = self.targets

        if args:
            filename = args.split(',')[0]
        else:
            filename = 'log.xml'

        if filename.startswith('/'):
            output_dir = os.path.dirname(filename)
            filename = os.path.basename(filename)
        else:
            output_dir = os.path.join(os.path.dirname(self.metadata.path), 'output')

        try:
            os.makedirs(output_dir)
        except OSError, error:
            if error.errno == errno.EEXIST:
                pass
        except Exception, error:
            out.critical('failed to create directories: %s' % str(error))
            return

        filename = os.path.join(output_dir, filename)

        if os.path.exists(filename):
            out.warning('file %s exists.' % filename)
            if not input('should i overwrite %s? (y/N) ' % filename, ['y', 'yes'], self.interactive):
                filename += '.' + timestamp()

        out.info('saving output to %s' % filename)

        try:
            outxml = open(filename, 'w')
        except IOError, error:
            out.error('failed to open file for writing: %s' % error.strerror)
            return

        output = XMLOutput()
        output.add_header(self.metadata)
        for target in targets:
            output.add_target(targets[target])

        outxml.write(output.pretty())
        outxml.close()

    def do_quit(self, args):
        """
        Disconnects from all hosts and exits the programm. If a bootarg
        argument is set, the hosts are either rebooted or powered off.
        The tester is asked to save the XML log when exiting MTUI.

        quit [bootarg]
        Keyword arguments:
        bootarg  -- reboot or poweroff
        """

        if not input('save log? (Y/n) ', ['n', 'no'], self.interactive):
            self.do_save(None)

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
        return [i for i in list(enabled_targets(self.targets)) + appendix if i.startswith(text) and i not in line]

    def complete_enabled_hostlist_with_all(self, text, line, begidx, endidx, appendix=[]):
        return [i for i in list(enabled_targets(self.targets)) + ['all'] + appendix if i.startswith(text) and i not in line]

    def complete_packagelist(self, text, line, begidx, endidx, appendix=[]):
        return [i for i in self.metadata.get_package_list() if i.startswith(text) and i not in line]

    def complete_testopia_testcaselist(self, text, line, begidx, endidx):
        try:
            assert(self.testopia.testcases)
        except (AttributeError, AssertionError):
            release = self.metadata.get_release()
            packages = self.metadata.get_package_list()
            self.testopia = Testopia(release, packages)

        testcases = [ i['summary'].replace(' ', '_') for i in self.testopia.testcases.values() ]
        return [i for i in testcases if i.startswith(text) and i not in line]

    def parse_error(self, method, args):
        print
        out.error('failed to parse command: %s %s' % (method.__name__.replace('do_', ''), args))
        print '%s: %s' % (method.__name__.replace('do_', ''), method.__doc__)


def script_hook(targets, which, templatedir, md5):
    # this hook seriously needs a rewrite

    if which not in ['post', 'pre', 'compare']:
        return

    output_dir = os.path.join(templatedir, 'output', 'scripts')
    remote_dir = os.path.join(config.target_tempdir, md5)

    scriptdir = os.path.join(templatedir, 'scripts', which)

    if not os.path.isdir(scriptdir):
        out.warning('%s scripts not found in %s' % (which, scriptdir))
        return

    for script in os.listdir(scriptdir):
        local_file = os.path.join(scriptdir, script)
        remote_file = '%s.%s' % (which, script)

        if not os.path.isfile(local_file):
            continue

        out.info('preparing script %s' % script)

        try:
            if which == 'compare':
                for target in targets:
                    suffix = script.rpartition('.')[2]
                    prename = '%s/pre.%s.%s' % (output_dir, script.replace('compare_', 'check_'), target)
                    postname = '%s/post.%s.%s' % (output_dir, script.replace('compare_', 'check_'), target)
                    prename = glob.glob(prename.replace('.%s.' % suffix, '*'))[0]
                    postname = glob.glob(postname.replace('.%s.' % suffix, '*'))[0]
                    comparescript = os.path.join(templatedir, 'scripts', 'compare', script)
                    command = [comparescript, prename, postname]

                    out.debug('running %s' % str(command))
                    stdout = stderr = None
                    try:
                        sub = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                        (stdout, stderr) = sub.communicate()
                        exitcode = sub.wait()
                    except Exception, error:
                        out.critical('running compare script failed: %s' % str(error))
                        exitcode = 1

                    if exitcode == 1:
                        out.critical('testcase %s failed: %s\n%s' % (script, str(command), stdout))
                        if stderr:
                            print 'stderr:', stderr

                    if exitcode == 2:
                        out.warning('internal error in testcase %s: %s' % (script, str(command)))

                    targets[target].log.append([' '.join(command), str(stdout), str(stderr), exitcode, 0])
            else:

                FileUpload(targets, local_file, os.path.join(remote_dir, remote_file)).run()
                RunCommand(targets, '%s/%s %s' % (remote_dir, remote_file, md5)).run()

                try:
                    os.makedirs(output_dir)
                except OSError, error:
                    if error.errno == errno.EEXIST:
                        pass
                except Exception, error:
                    out.critical('failed to create directories: %s' % str(error))
                    return

                for target in targets:
                    filename = os.path.join(output_dir, '%s.%s' % (remote_file, target))
                    try:
                        f = open(filename, 'w')
                        f.write(targets[target].lastout())
                        f.write(targets[target].lasterr())
                        f.close()
                    except IOError, error:
                        out.error('failed to write script output to %s: %s' % (filename, error.strerror))
        except KeyboardInterrupt:
            out.warning('skipping script %s' % script)
            continue

def enabled_targets(targets):
    temporary_targets = {}

    for target in targets:
        try:
            if targets[target].state != 'disabled':
                temporary_targets[target] = targets[target]
        except KeyError:
            out.warning('host %s not in database' % target)

    return temporary_targets


def selected_targets(targets, target_list):
    temporary_targets = {}

    for target in target_list:
        try:
            temporary_targets[target] = targets[target]
        except KeyError:
            out.warning('host %s not in database' % target)

    return temporary_targets

def user_deprecation(out, msg):
    out.warning(msg)
