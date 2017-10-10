# -*- coding: utf-8 -*-

import os
from os.path import abspath, join, basename, dirname
from errno import ENOENT
from errno import EEXIST
import shutil
import glob
import stat
from traceback import format_exc
from abc import ABCMeta
from abc import abstractmethod
from urllib.request import urlopen
import re
import subprocess

from mtui.target import HostsGroup
from mtui.target import Target
from mtui.refhost import RefhostsFactory
from mtui.refhost import Attributes
from mtui.testopia import Testopia


from mtui import utils

from mtui.utils import ensure_dir_exists, chdir
from mtui.types.obs import RequestReviewID
from mtui.messages import SvnCheckoutInterruptedError
from mtui import updater
from mtui.parsemeta import OBSMetadataParser
from mtui.utils import nottest
from paramiko.ssh_exception import SSHException, NoValidConnectionsError, ChannelException
from mtui.target.actions import UpdateError
from mtui.connector.smelt_obs import SMELT


class _TemplateIOError(IOError):

    """
    New type to distinguish between IOErrors happening when reading the
    template file which are recoverable and IOErrors happening somewhere
    else in the process
    """
    pass


def testreport_svn_checkout(config, log, path, id):
    ensure_dir_exists(
        config.template_dir,
        on_create=lambda path: log.debug(
            'created config.template_dir directory {0}'.format(path)))

    uri = join(path, id)
    with chdir(config.template_dir):
        try:
            subprocess.check_call(['svn', 'co', uri])
        except KeyboardInterrupt:
            raise SvnCheckoutInterruptedError(uri)


class UpdateID(object):

    def __init__(self, id_, testreport_factory, testreport_svn_checkout):
        self.id = id_
        self.smelt = None
        self.testreport_factory = testreport_factory
        self._vcs_checkout = testreport_svn_checkout

    def make_testreport(self, config, logger, autoconnect=True):
        tr = self.testreport_factory(
            config,
            logger,
        )
        trpath = join(config.template_dir, str(self.id), 'log')

        try:
            tr.read(trpath)
        except _TemplateIOError as e:
            if e.errno != ENOENT:
                raise

            self._vcs_checkout(
                config,
                logger,
                config.svn_path,
                str(self.id))

            tr.read(trpath)

        if autoconnect:
            tr.connect_targets()

        tr.smelt = self.smelt
        if self.smelt:
            tr.smelt.logger = logger

        return tr


class OBSUpdateID(UpdateID):

    def __init__(self, rrid, *args, **kw):
        super(OBSUpdateID, self).__init__(
            RequestReviewID(rrid),
            OBSTestReport,
            testreport_svn_checkout
        )

        self.smelt = SMELT(self.id.maintenance_id)


class TestReportAlreadyLoaded(RuntimeError):
    pass


@nottest
class TestReport(object, metaclass=ABCMeta):
    # FIXME: the code around read() (_open_and_parse, _parse and factory
    # _factory_md5) is weird a lot.
    # Firstly, it might clear some things up to change the open/read
    # things to file-like interface.

    refhostsFactory = RefhostsFactory

    @property
    @abstractmethod
    def _type(self):
        """
        :return: str Short human readable description of the TestReport
            type.
        """

    def __init__(self, config, log, scripts_src_dir=None):
        self.config = config
        self.log = log

        self._scripts_src_dir = scripts_src_dir or join(
            config.datadir,
            'scripts')
        self.directory = config.template_dir

        # Note: the default values here are unchanged from the previous
        # class Metadata for backward compaibility purposes, so we don't
        # have to modify every user of this class at the same time as
        # refactoring the internals.
        self.path = ''
        """
        :type path: str or None
        :param path: path to the testreport file if loaded, otherwise None
        """

        self.packages = {}
        self.systems = {}
        """
        :type systems: dict str -> str
        :param systems: hostname -> system
        """
        self.targets = HostsGroup([])
        """
        :type  targets: dict(hostname = L{Target})
            where hostname = str
        """
        self.bugs = {}
        self.testplatforms = []
        self.category = ""
        self.packager = ""
        self.reviewer = ""
        self.repository = None

        self._attrs = [
            'category',
            'packager',
            'reviewer',
            'packages',
            'bugs',
            'repository',
        ]
        """
        :type attrs: [str]
        :param attrs: attributes expected to exist on `self` after
            parsing the template
        """

        self.testopia = None
        """
        :type testopia: L{Testopia}
        """

    @staticmethod
    def _copytree(*args, **kw):
        return shutil.copytree(*args, **kw)

    def _open_and_parse(self, path):
        try:
            with open(path, 'r') as f:
                self._parse(f)
        except IOError as e:
            args = list(e.args) + [e.filename]
            e_new = _TemplateIOError(*args)
            e_new.__cause__ = e  # PEP 3134
            raise e_new

    def read(self, path):
        self._open_and_parse(path)
        self.path = abspath(path)

        if self.config.chdir_to_template_dir:
            os.chdir(dirname(path))

        self.copy_scripts()
        self.create_installogs_dir()

        for tp in self.testplatforms:
            self._refhosts_from_tp(tp)

    @abstractmethod
    def _parser(self):
        """
        :returns: L{MetadataParser}
        """

    def _parse(self, tpl):
        """
        Parse qam testreport template into self attributes

        :type tpl_: file like object
        :param tpl_: opened template to read
        """

        if self.path:
            raise TestReportAlreadyLoaded(self.path)

        parser = self._parser()

        for line in tpl.readlines():
            parser.parse_line(self, line)

        self._warn_missing_fields()

    def _warn_missing_fields(self):
        missing = [x for x in self._attrs if not getattr(self, x)]
        if missing:
            msg = "TestReport: missing fields: {0}"
            self.log.warning(msg.format(missing))

    def get_package_list(self):
        return list(self.packages.keys())

    def get_release(self):
        return utils.get_release(list(self.systems.values()))

    def _get_doer(self, registry):
        return registry[self._get_updater_id()]

    @abstractmethod
    def _get_updater_id(self):
        """
        :return: str Identifier of adaptee to use from `mtui.updater`
        """

    def get_preparer(self):
        return self._get_doer(updater.Preparer)

    def get_updater(self):
        return self._get_doer(updater.Updater)

    def get_installer(self):
        return self._get_doer(updater.Installer)

    def get_uninstaller(self):
        return self._get_doer(updater.Uninstaller)

    def get_downgrader(self):
        return self._get_doer(updater.Downgrader)

    def list_update_commands(self, targets, display):
        '''
        :type  targets: dict(hostname = L{Target})
            where hostname = str
        :display: callable(str -> None)
        '''
        updater = self.get_updater()

        display('\n'.join(updater(
            self.log,
            targets,
            self.get_package_list(),
            self).commands
        ))
        del updater

    def perform_get(self, targets, remote):
        local = self.report_wd('downloads', basename(remote), filepath=True)

        targets.get(remote, local)

    def perform_prepare(self, targets, **kw):
        preparer = self.get_preparer()
        preparer(
            self.log,
            targets,
            self.get_package_list(),
            self,
            **kw
        ).run()

    def perform_update(self, targets, params):
        '''
        :type  targets: dict(hostname = L{Target})
            where hostname = str
        '''
        targets.add_history(
            ['update', str(self.id), ' '.join(self.get_package_list())])

        updater = self.get_updater()
        self.log.debug("chosen updater: {!r}".format(updater))
        try:
            updater(
                self.log,
                targets,
                self.get_package_list(),
                self).run(params)
        except UpdateError as e:
            self.log.error('Update failed: {!s}'.format(e))
            self.log.warning('Error while updating. Rolling back changes')
            self.perform_downgrade(targets)

    def perform_downgrade(self, targets):
        targets.add_history(
            ['downgrade', str(self.id), ' '.join(self.get_package_list())])

        downgrader = self.get_downgrader()
        downgrader(
            self.log,
            targets,
            self.get_package_list(),
            self).run()

    def perform_install(self, targets, packages):
        targets.add_history(['install', packages])

        tool = self.get_installer()
        tool(self.log, targets, packages).run()

    def perform_uninstall(self, targets, packages):
        tool = self.get_uninstaller()
        tool(self.log, targets, packages).run()

    def copy_scripts(self):
        if not self.path:
            raise RuntimeError("Called while missing path")

        # copy check_* and compare_* scripts to the template directory
        # TODO: do not override
        src = self._scripts_src_dir
        dst = self.scripts_wd()

        ignore = shutil.ignore_patterns('*.svn')

        self._copy_scripts(src, dst, ignore)
        self._ensure_executable('{0}/*/compare_*'.format(dst))

    def _copy_scripts(self, src, dst, ignore):
        try:
            self.log.debug("Copying scripts: {0} -> {1}".format(
                src, dst
            ))
            self._copytree(src, dst, ignore=ignore)
        except OSError as e:
            # this should not happen but was already noticed once or
            # twice.  probable due to nfs timeouts if mtui was checked
            # out to a nfs mount.
            msg = "Copy scripts {0} -> {1} failed. reason:"
            msg = msg.format(src, dst)
            if e.errno == ENOENT:
                self.log.error(msg)
                self.log.error(str(e))
                self.log.error("copy scripts manually")
                self.log.debug(format_exc())
            elif e.errno == EEXIST:
                self.log.warning(msg)
                self.log.warning(str(e))
                self.log.debug(format_exc())
            else:
                raise

    def create_installogs_dir(self):
        if not self.path:
            raise RuntimeError("Called while missing path")

        self._create_installogs_dir()

    def _create_installogs_dir(self):
        os.makedirs(
            join(
                self.config.template_dir,
                str(self.id),
                self.config.install_logs),
            exist_ok=True)

    @staticmethod
    def _ensure_executable(pattern):
        for i in glob.glob(pattern):
            # make sure the compare scripts (which run localy) are
            # executable
            # TODO: add test that the scripts indeed are +x
            st = os.stat(i)
            os.chmod(i, st.st_mode | stat.S_IEXEC)

    def connect_targets(self, make_target=Target):
        targets = {}
        new_systems = {}
        for (host, system) in list(self.systems.items()):
            try:
                targets[host] = make_target(
                    self.config,
                    host,
                    system,
                    self.get_package_list(),
                    logger=self.log,
                    timeout=self.config.connection_timeout)
                targets[host].add_history(['connect'])
                new_systems[host] = system
            except Exception:
                self.log.debug(format_exc())
                msg = 'failed to add host {0} to target list'
                self.log.warning(msg.format(host))
            except KeyboardInterrupt:
                # skip adding the reference host if CTRL-C was pressed.
                # FIXME: this might not work if we are somewhere deep in
                # the network/ssh code where KeyboardInterrupt is not
                # thrown.
                # Note: this wouldn't be a problem with Twisted by
                # default.
                # With paramiko we'd have to run it in threads, assuming
                # the network/ssh code really can't KeyboardInterrupt
                self.log.warning('skipping host {0}'.format(host))

        # We need to be sure that only the system property only have the  connected hosts
        self.systems = new_systems
        for t in self.targets:
            del self.targets[t]
        self.targets.update(targets)

    def add_target(self, hostname, system):
        if hostname in self.targets:
            self.log.warning('already connected to {0}. skipping.'.format(
                self.targets[hostname].hostname
            ))
            return
        try:
            self.targets[hostname] = Target(
                self.config,
                hostname,
                system,
                self.get_package_list(),
                logger=self.log,
            )

            if self:
                self.systems[hostname] = system
        except (SSHException, NoValidConnectionsError, ChannelException):
            self.log.warning('failed to add host {0} to target list'.format(hostname))
            self.log.debug(format_exc())

    def _refhosts_from_tp(self, testplatform):
        refhosts = self.refhostsFactory(self.config, self.log)

        try:
            hostnames = refhosts.search(Attributes.from_testplatform(
                testplatform, self.log
            ))
        except (ValueError, KeyError):
            hostnames = []
            msg = 'failed to parse testplatform {0!r}'
            self.log.warning(msg.format(testplatform))
        else:
            if not hostnames:
                msg = 'nothing found for testplatform {0!r}'
                self.log.warning(msg.format(testplatform))

        self.systems.update(dict(
            [(hn, refhosts.get_host_systemname(hn)) for hn in hostnames]
        ))

    def list_bugs(self, sink, arg):
        return sink(self.bugs, arg)

    def _show_yourself_data(self):
        return [
            ('Category', self.category),
            ('Hosts', ' '.join(sorted(self.systems.keys()))),
            ('Reviewer', self.reviewer),
            ('Packager', self.packager),
            ('Bugs', ', '.join(sorted(self.bugs.keys()))),
            ('Packages', ' '.join(sorted(self.get_package_list()))),
            ('Testreport', self._testreport_url()),
            ('Repository', self.repository), ] + [('Testplatform', x) for x in self.testplatforms]

    def show_yourself(self, writer):
        self._aligned_write(writer, self._show_yourself_data())

    @staticmethod
    def _aligned_write(writer, data):
        """
        :type data:  [(str, str)]
        :param data: (key, value)
        """
        for x in sorted(data):
            writer.write("{0:15}: {1}\n".format(*x))

    def _testreport_url(self):
        return '/'.join([self.config.reports_url, str(self.id), 'log'])

    def local_wd(self, *paths):
        """
        :return: str local working directory
        """
        return self._wd(self.config.local_tempdir, str(self.id), *paths)

    def report_wd(self, *paths, **kw):
        """
        :return: str local working directory relative to the testreport
            checkout.
        """
        assert self.path, "empty path"

        return self._wd(dirname(self.path), *paths, **kw)

    @staticmethod
    def _wd(*paths, **kwargs):
        return ensure_dir_exists(*paths, **kwargs)

    def target_wd(self, *paths):
        """
        :return: str remote working directory on SUT
        """
        return join(self.config.target_tempdir, str(self.id), *paths)

    def scripts_wd(self, *paths):
        """
        :return: str path to the scripts dir joined with paths

        Note this method does not create the directories as needed
        because that's handled by L{TestReport.copy_scripts}
        """
        return join(self.report_wd(), *["scripts"] + list(paths))

    def get_testsuite_comment(self, testsuite, date):
        return 'testing {!s} on {!s} on {!s}'.format(
            testsuite,
            "{!s} {!s}".format(self._type, self.id),
            date,
        )

    def __repr__(self):
        return "<{0}.{1} {2}>".format(
            self.__module__,
            self.__class__.__name__,
            self.id
        )

    def run_scripts(self, s, targets):
        """
        :type s: L{Script} class
        """

        d = self.scripts_wd(s.subdir)

        for r, _, fs in os.walk(d):
            if r == d:
                for f in fs:
                    x = s(self, join(d, f), self.log)
                    x.run(targets)

    def download_file(self, from_, into):
        self.log.info("Downloading {!s}".format(from_))
        from contextlib import closing
        with open(into, 'wb') as dst, closing(urlopen(from_)) as src:
            dst.writelines(src)

    def load_testopia(self, *packages):
        try:
            assert(self.testopia.testcases and not packages)
        except (AttributeError, AssertionError):
            self.testopia = Testopia(
                self.config,
                self.log,
                self.get_release(),
                packages or self.get_package_list(),
            )

        return self.testopia

    def list_versions(self, sink, targets, packages):
        query = r'''
            for p in {!s}; do \
                zypper -n search -s --match-exact -t package $p; \
            done \
            | grep -e ^[iv] \
            | awk -F '|' '{{ print $2 $4 }}' \
            | sort -u
        '''

        packages = packages or self.get_package_list()

        targets.run(query.format(' '.join(packages)))

        # this is a bit convoluted because the data is aggregated
        # on display (see the example in CommandPrompt#do_list_versions)
        # but acquired piecemeal in random order.
        #
        # input for a single target:
        #
        #   line = PKKGNAME +SP PKGVER
        #   input = *(line EOL)

        # by_host_pkg[hostname][package] = [version, ...]
        by_host_pkg = dict()
        for hn, t in list(targets.items()):
            by_host_pkg[hn] = dict()
            for line in t.lastout().split('\n'):
                match = re.search('(\S+)\s+(\S+)', line)
                if not match:
                    continue
                pkg, ver = match.group(1), match.group(2)
                by_host_pkg[hn].setdefault(pkg, []).append(ver)

        # by_pkg_vers[package][(version, ...)] = [hostname, ...]
        by_pkg_vers = dict()
        for hn, pvs in list(by_host_pkg.items()):
            for pkg, vs in list(pvs.items()):
                by_pkg_vers.setdefault(
                    pkg,
                    dict()).setdefault(
                    tuple(vs),
                    []).append(hn)

        # by_hosts_pkg[(hostname, ...)] = [(package, (version, ...)), ...]
        by_hosts_pkg = dict()
        for pkg, vshs in list(by_pkg_vers.items()):
            for vs, hs in list(vshs.items()):
                by_hosts_pkg.setdefault(tuple(hs), []).append((pkg, vs))

        return sink(targets, by_hosts_pkg)

    def generate_templatefile(self, xmllog):
        from mtui.export import fill_template
        return fill_template(self.id,
            self.log,
            self.path,
            xmllog,
            self.config,
            self.smelt
        )

    def generate_install_logs(self, xmllog, host):
        from mtui.export import xml_installog_to_template
        return xml_installog_to_template(
            self.log,
            xmllog,
            self.config,
            host)

    def generate_xmllog(self, targetHosts=None):
        from mtui.xmlout import XMLOutput

        output = XMLOutput()

        if self:
            output.add_header(self)

        targets = list(self.targets.values())
        if targetHosts is not None:
            targets = list(targetHosts)

        for t in targets:
            output.add_target(t)

        return output.pretty()


class NullTestReport(TestReport):
    _type = "No"

    def __init__(self, config, log, *a, **kw):
        super(NullTestReport, self).__init__(config, log, *a, **kw)
        self.id = None
        self.path = join(os.getcwd(), "None")

    def __bool__(tr):
        return False

    def target_wd(self, *paths):
        return join(self.config.target_tempdir, *paths)

    def _get_updater_id(tr):
        return None

    def _parser(tr):
        return None


class OBSTestReport(TestReport):
    _type = "OBS"

    def __init__(self, *a, **kw):
        super(OBSTestReport, self).__init__(*a, **kw)

        self.rrid = None
        self.rating = None

        self._attrs += [
            'rrid',
            'rating',
        ]

    @property
    def id(self):
        return self.rrid

    def _get_updater_id(self):
        return self.get_release()

    def _parser(self):
        return OBSMetadataParser()

    def _show_yourself_data(self):
        return [
            ('ReviewRequestID', self.rrid),
            ('Rating', self.rating),
        ] + super(OBSTestReport, self)._show_yourself_data()

    def set_repo(self, target, operation):
        if operation == 'add':
            target.run_repose('issue-add', self.report_wd())
        elif operation == 'remove':
            target.run_repose('issue-rm', str(self.rrid))
        else:
            raise ValueError(
                "Not supported repose operation {}".format(operation))
