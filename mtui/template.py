# -*- coding: utf-8 -*-

import os
import re
from os.path import join, dirname
from errno import ENOENT
from errno import EEXIST
import shutil
import glob
import stat
from traceback import format_exc
from abc import ABCMeta
from abc import abstractmethod
from datetime import date

from mtui.target import Target
from mtui.refhost import RefhostsFactory
from mtui.utils import ensure_dir_exists, chdir
from mtui.types import MD5Hash
from mtui.types.obs import RequestReviewID
from mtui.utils import edit_text
from mtui.utils import UserMessage

try:
    from nose.tools import nottest
    has_nose = True
except ImportError:
    has_nose = False

class _TemplateIOError(IOError):
    """
    New type to distinguish between IOErrors happening when reading the
    template file which are recoverable and IOErrors happening somewhere
    else in the process
    """
    pass

class QadbReportCommentLengthWarning(UserMessage):
    message = 'comment strings > 100 chars are truncated by remote_qa_db_report.pl'

def testreport_svn_checkout(config, log, uri):
    ensure_dir_exists(
        config.template_dir,
        on_create=lambda path: log.debug('created config.template_dir directory {0}'.format(path))
    )

    with chdir(config.template_dir):
        # FIXME: use python module to perform svn checkout
        os.system('svn co {0}'.format(uri))

class UpdateID(object):
    def __init__(self, id_, testreport_factory, testreport_svn_checkout):
        self.id = id_
        self.testreport_factory = testreport_factory
        self._vcs_checkout = testreport_svn_checkout

    def _template_path(self):
        return join(self.config.template_dir, str(self.id), 'log')

    def make_testreport(self):
        tr = self.testreport_factory(
            self.config,
            self.log,
            date = date
        )

        try:
            tr.read(self._template_path())
        except _TemplateIOError as e:
            if e.errno != ENOENT:
                raise

            self._vcs_checkout(
                self.config,
                self.log,
                join(self.config.svn_path, str(self.id))
            )

            tr.read(self._template_path())

        return tr

class SwampUpdateID(UpdateID):
    def __init__(self, md5):
        """
        :param md5: str
        """
        super(SwampUpdateID, self).__init__(
            MD5Hash(md5),
            SwampTestReport,
            testreport_svn_checkout
        )

class OBSUpdateID(UpdateID):
    def __init__(self, rrid, *args, **kw):
        super(OBSUpdateID, self).__init__(
            RequestReviewID(rrid),
            OBSTestReport,
            testreport_svn_checkout
        )

class TestReportAlreadyLoaded(RuntimeError):
    pass

class TestReport(object):
    # FIXME: the code around read() (_open_and_parse, _parse and factory
    # _factory_md5) is weird a lot.
    # Firstly, it might clear some things up to change the open/read
    # things to file-like interface.

    __metaclass__ = ABCMeta

    targetFactory = Target
    refhostsFactory = RefhostsFactory

    @property
    @abstractmethod
    def _type(self):
        """
        :return: str Short human readable description of the TestReport
            type.
        """

    def __init__(self, config, log, date):
        """
        :type today: f :: L{datetime.date}
        """
        self.config = config
        self.log = log
        self._date = date

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

        self.patches = {}
        self.packages = {}
        self.systems = {}
        """
        :type systems: dict str -> str
        :param systems: hostname -> system
        """
        self.bugs = {}
        self.testplatforms = []
        self.category = ""
        self.swampid = ""
        self.packager = ""
        self.reviewer = ""
        self.md5 = None
        """
        :type md5: MD5Hash instance or None
        """


    def _copytree(_, *args, **kw):
        return shutil.copytree(*args, **kw)

    def _open_and_parse(self, path):
        try:
            with open(path, 'r') as f:
                self._parse(f)
        except IOError as e:
            args = list(e.args) + [e.filename]
            e_new = _TemplateIOError(*args)
            e_new.__cause__ = e # PEP 3134
            raise e_new

    def read(self, path):
        self._open_and_parse(path)
        self.path = path

        if self.config.chdir_to_template_dir:
            os.chdir(dirname(path))

        self.copy_scripts()

    def _parse(self, tpl):
        """
        Parse qam testreport template into self attributes

        :type tpl_: file like object
        :param tpl_: opened template to read
        """

        if self.path:
            raise TestReportAlreadyLoaded(self.path)

        for line in tpl.readlines():
            match = re.search('MD5 sum: (.+)', line)
            if match:
                self.md5 = MD5Hash(match.group(1))

            match = re.search('Category: (.+)', line)
            if match:
                self.category = match.group(1)

            match = re.search('YOU Patch No: (\d+)', line)
            if match:
                self.patches['you'] = match.group(1)

            match = re.search('ZYPP Patch No: (\d+)', line)
            if match:
                self.patches['zypp'] = match.group(1)

            match = re.search('SAT Patch No: (\d+)', line)
            if match:
                self.patches['sat'] = match.group(1)

            match = re.search('RES Patch No: (\d+)', line)
            if match:
                self.patches['res'] = match.group(1)

            match = re.search('SUBSWAMPID: (\d+)', line)
            if match:
                self.swampid = match.group(1)

            match = re.search('Packager: (.+)', line)
            if match:
                self.packager = match.group(1)

            match = re.search('Packages: (.+)', line)
            if match:
                self.packages = dict([(pack.split()[0], pack.split()[2]) for pack in match.group(1).split(',')])

            match = re.search('Test Plan Reviewers: (.+)', line)
            if match:
                self.reviewer = match.group(1)

            match = re.search('Bug #(\d+) \("(.*)"\):', line)  # deprecated
            if match:
                self.bugs[match.group(1)] = match.group(2)

            match = re.search('Testplatform: (.*)', line)
            if match:
                self.testplatforms.append(match.group(1))

            match = re.search('(.*-.*) \(reference host: (\S+).*\)', line)
            if match:
                if '?' not in match.group(2):
                    self.systems[match.group(2)] = match.group(1)

            match = re.search('Bugs: (.*)', line)
            if match:
                for bug in match.group(1).split(','):
                    self.bugs[bug.strip(' ')] = 'Description not available'


        attrs = [
            'md5',
            'category',
            'swampid',
            'packager',
            'reviewer',
            'patches',
            'packages',
            'systems',
            'bugs',
        ]
        missing = [x for x in attrs if not getattr(self, x)]
        if missing:
            msg = "TestReport: missing fields: {0}"
            self.log.warning(msg.format(missing))

    def get_package_list(self):
        return self.packages.keys()

    def get_release(self):
        systems = ' '.join(self.systems.values())
        if re.search('rhel', systems):
            return 'YUM'
        if re.search('manager', systems):
            # SUSE Manager hosts should have the sle11 zypper stack,
            # even on sle10 installations
            return '11'
        if re.search('mgr', systems):
            return '11'
        if re.search('sles4vmware', systems):
            return '11'
        if re.search('cloud', systems):
            return '11'
        if re.search('studio', systems):
            return '11'
        if re.search('slms', systems):
            return '11'
        if re.search('sle.11', systems):
            return '11'
        if re.search('sle.10', systems):
            return '10'
        if re.search('sle.9', systems):
            return '9'
        if re.search('sl11', systems):
            return '114'

    def copy_scripts(self):
        if not self.path:
            raise RuntimeError("Called while missing path")

        # copy check_* and compare_* scripts to the template directory
        # TODO: do not override
        src = join(self.config.datadir, 'scripts')
        dst = join(dirname(self.path), 'scripts')

        ignore = shutil.ignore_patterns('*.svn')

        self._copy_scripts(src, dst, ignore)
        self._ensure_executable('{0}/*/compare_*'.format(dst))

    def _copy_scripts(self, src, dst, ignore):
        try:
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

    def _ensure_executable(self, pattern):
        for i in glob.glob(pattern):
            # make sure the compare scripts (which run localy) are
            # executable
            # TODO: add test that the scripts indeed are +x
            st = os.stat(i)
            os.chmod(i, st.st_mode | stat.S_IEXEC)

    def connect_targets(self):
        # TODO: duplicated in:
        #   autoadd
        #   add_host
        #   load_template
        targets = {}

        for (host, system) in self.systems.items():
            try:
                targets[host] = self.targetFactory(host, system,
                    self.get_package_list(),
                    timeout=self.config.connection_timeout)
                targets[host].add_history(['connect'])
            except Exception as e:
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

        return targets

    def _refhosts_from_tp(self, testplatform):
        refhosts = self.refhostsFactory(self.config, self.log)

        try:
            refhosts.set_attributes_from_testplatform(testplatform)
            hostnames = refhosts.search()
        except (ValueError, KeyError):
            hostnames = []
            msg = 'failed to parse testplatform {0!r}'
            self.log.warning(msg.format(testplatform))
        else:
            if not hostnames:
                msg = 'nothing found for testplatform {0!r}'
                self.log.warning(msg.format(testplatform))

        return dict([(hn, refhosts.get_host_systemname(hn))
                    for hn in hostnames])

    def load_systems_from_testplatforms(self):
        for tp in self.testplatforms:
            self.systems.update(self._refhosts_from_tp(tp))

    def add_host(self, hostname, system):
        self.systems[hostname] = system

    def _show_yourself_data(self):
        return [
            ('Category'  , self.category),
            ('Hosts'     , ' '.join(sorted(self.systems.keys()))),
            ('Reviewer'  , self.reviewer),
            ('Packager'  , self.packager),
            ('Bugs'      , ', '.join(self.bugs.keys())),
            ('Packages'  , ' '.join(sorted(self.get_package_list()))),
            ('Testreport', self._testreport_url()),
        ] + [(x.upper(), y) for x,y in self.patches.items()]

    def show_yourself(self, writer):
        self._aligned_write(writer, dict(self._show_yourself_data()))

    def _aligned_write(self, writer, data):
        """
        :type data: dict
        """
        fmt = "{0:15}: {1}\n"
        xs = data.items()
        xs.sort()
        for x in xs:
            writer.write(fmt.format(*x))

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
        return self._wd(dirname(self.path), *paths, **kw)

    def _wd(self, *paths, **kwargs):
        return ensure_dir_exists(*paths, **kwargs)

    def target_wd(self, *paths):
        """
        :return: str remote working directory on SUT
        """
        return join(self.config.target_tempdir, str(self.id), *paths)

    def patchinfo_url(self):
        return '/'.join([self.config.patchinfo_url, str(self.id)])

    def get_testsuite_comment(self, testsuite):
        return TestsuiteComment(
            self.log,
            "{0} {1}".format(self._type, self.id),
            testsuite,
            self._date.today(),
            text_editor = edit_text
        )

class TestsuiteComment(object):
    _max_comment_len = 100

    def __init__(self, log, update_id, testsuite, date, text_editor = None):
        """
        :type update_id: str
        :type testsuite: str or None
        :type date: L{datetime.date}
        :type text_editor: f :: str -> str
        """
        self.update_id = update_id
        self.log = log
        self.date = date
        self.testsuite = testsuite

        self._user_str = None
        self._text_editor = text_editor

    def _to_str(self):
        if self._user_str:
            return self._user_str

        return 'testing {2} on {0} on {1}'.format(
            self.update_id,
            self.date.strftime('%d/%m/%y'),
            self.testsuite
        )

    def __str__(self):
        xs = self._to_str()
        if len(xs) > self._max_comment_len:
            self.log.warning(QadbReportCommentLengthWarning())
        return xs

    def edit_text(self):
        self._user_str = self._text_editor(str(self))

class SwampTestReport(TestReport):
    _type = "SWAMP"

    @property
    def id(self):
        return self.md5

    def _show_yourself_data(self):
        return [
            ('MD5SUM'  , self.md5),
            ('SWAMP ID', self.swampid),
            ('Build'   ,'/'.join([self.config.patchinfo_url, str(self.md5)])),
        ] + super(SwampTestReport, self)._show_yourself_data()

class OBSTestReport(TestReport):
    _type = "OBS"

    @property
    def id(self):
        return self.rrid

if has_nose:
    TestReport = nottest(TestReport)
