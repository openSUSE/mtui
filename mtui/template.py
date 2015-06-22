# -*- coding: utf-8 -*-

import os
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
import subprocess

from mtui.target import Target
from mtui.target import TargetI
from mtui.target import RunCommand
from mtui.target import FileUpload
from mtui.refhost import RefhostsFactory
from mtui.refhost import Attributes
from mtui.testopia import Testopia

from mtui.utils import ensure_dir_exists, chdir
from mtui.types import MD5Hash
from mtui.types.obs import RequestReviewID
from mtui.utils import edit_text
from mtui.five import with_metaclass
from mtui.messages import QadbReportCommentLengthWarning
from mtui.messages import FailedToDownloadSrcRPMError
from mtui.messages import FailedToExtractSrcRPM
from mtui.messages import SrcRPMExtractedMessage
from mtui import messages
from mtui.messages import SvnCheckoutInterruptedError
from mtui import updater
from mtui.parsemeta import OBSMetadataParser, SWAMPMetadataParser
from mtui.utils import ass_is, ass_isL
from mtui.utils import nottest

class _TemplateIOError(IOError):
    """
    New type to distinguish between IOErrors happening when reading the
    template file which are recoverable and IOErrors happening somewhere
    else in the process
    """
    pass

def download_source_rpm(uri, recursion_level, system = subprocess.call):
    cmd = 'wget -q -r -nd -l{1} --no-parent -A "*src.rpm" {0}/'.format(
        uri,
        recursion_level
    )
    rc = system(cmd, shell = True)

    if rc:
        raise FailedToDownloadSrcRPMError(rc, cmd)

def testreport_svn_checkout(config, log, uri):
    ensure_dir_exists(
        config.template_dir,
        on_create=lambda path: log.debug('created config.template_dir directory {0}'.format(path))
    )

    with chdir(config.template_dir):
        # FIXME: use python module to perform svn checkout
        try:
            subprocess.check_call(['svn', 'co', uri])
        except KeyboardInterrupt:
            raise SvnCheckoutInterruptedError(uri)

class Scripts(object):
    def __init__(self, scripts):
        """
        :type scripts: [L{Script}]
        """
        self.scripts = scripts

    def run(self, targets):
        ass_isL(targets, TargetI)

        for x in self.scripts:
            x.run(targets)

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

@nottest
class TestReport(with_metaclass(ABCMeta, object)):
    # FIXME: the code around read() (_open_and_parse, _parse and factory
    # _factory_md5) is weird a lot.
    # Firstly, it might clear some things up to change the open/read
    # things to file-like interface.

    targetFactory = Target
    refhostsFactory = RefhostsFactory

    @property
    @abstractmethod
    def _type(self):
        """
        :return: str Short human readable description of the TestReport
            type.
        """

    def __init__(self, config, log, date, file_uploader = FileUpload,
    cmd_runner = RunCommand, scripts_src_dir = None):
        """
        :type today: f :: L{datetime.date}
        """
        self.config = config
        self.log = log
        self._date = date

        self.file_uploader = file_uploader
        self.cmd_runner = cmd_runner

        self._scripts_src_dir = scripts_src_dir
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
        self.targets = {}
        """
        :type  targets: dict(hostname = L{Target})
            where hostname = str
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
        self.repository = None

        self._attrs = [
            'category',
            'packager',
            'reviewer',
            'packages',
            'systems',
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
        return updater.get_release(list(self.systems.values()))

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
            targets,
            self.patches,
            self.get_package_list(),
            self).commands
        ))
        del updater

    def perform_prepare(self, targets, **kw):
        preparer = self.get_preparer()
        preparer(
            targets,
            self.get_package_list(),
            self,
            **kw
        ).run()

    def perform_update(self, targets):
        '''
        :type  targets: dict(hostname = L{Target})
            where hostname = str
        '''
        updater = self.get_updater()
        self.log.debug("chosen updater: %s" % repr(updater))
        updater(targets, self.patches, self.get_package_list(), self).run()

    def perform_downgrade(self, targets):
        tool = self.get_downgrader()
        tool(targets, self.get_package_list(), self.patches).run()

    def perform_install(self, targets, packages):
        tool = self.get_installer()
        tool(targets, packages).run()

    def perform_uninstall(self, targets, packages):
        tool = self.get_uninstaller()
        tool(targets, packages).run()

    def scripts_src_dir(self):
        if self._scripts_src_dir:
            return self._scripts_src_dir

        return join(self.config.datadir, 'scripts')

    def copy_scripts(self):
        if not self.path:
            raise RuntimeError("Called while missing path")

        # copy check_* and compare_* scripts to the template directory
        # TODO: do not override
        src = self.scripts_src_dir()
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

        for t in self.targets:
            del self.targets[t]
        self.targets.update(targets)

    def _refhosts_from_tp(self, testplatform):
        refhosts = self.refhostsFactory(self.config, self.log)

        try:
            hostnames = refhosts.search(Attributes.from_testplatform(
                  testplatform
                , self.log
            ))
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
            ('Bugs'      , ', '.join(sorted(self.bugs.keys()))),
            ('Packages'  , ' '.join(sorted(self.get_package_list()))),
            ('Testreport', self._testreport_url()),
            ('Repository', self.repository),
        ] + [(x.upper(), y) for x,y in self.patches.items()
        ] + [('Testplatform', x) for x in self.testplatforms
        ]

    def show_yourself(self, writer):
        self._aligned_write(writer, self._show_yourself_data())

    def _aligned_write(self, writer, data):
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

    def _wd(self, *paths, **kwargs):
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

    def downloads_wd(self, *path, **kw):
        """
        :return: str directory for downloads.
            If template is loaded, it's ${report directory}/downloads
            Otherwise ${CWD}/downloads
        """
        path = ['downloads'] + list(path)
        return self.report_wd(*path, **kw)

    def pkg_list_file(self):
        return self.report_wd('packages-list.txt', filepath = True)

    def get_testsuite_comment(self, testsuite):
        return TestsuiteComment(
            self.log,
            "{0} {1}".format(self._type, self.id),
            testsuite,
            self._date.today(),
            text_editor = edit_text
        )

    def __repr__(self):
        return "<{0}.{1} {2}>".format(
            self.__module__,
            self.__class__.__name__,
            self.id
        )

    def script_hooks(self, s):
        """
        :type s: L{Script} class
        """

        d = s.absolute_subdir(self)

        return Scripts([
            s(self, join(d, x), self.log, self.file_uploader, self.cmd_runner)
            for r, _, fs in os.walk(d) if r == d
            for x in fs
        ])

    def download_source_rpm(self):
        raise NotImplementedError()

    def extract_source_rpm(self):
        with chdir(self.local_wd()):
            self.download_source_rpm()

            cmd = 'for i in *src.rpm; do name=$(rpm -qp --queryformat "%{NAME}" $i); mkdir -p $name; cd $name; rpm2cpio ../$i | cpio -i --unconditional --preserve-modification-time --make-directories; cd ..; done'
            rc = os.system(cmd)

            if rc:
                raise FailedToExtractSrcRPM(rc, cmd)

        self.log.info(SrcRPMExtractedMessage(self.local_wd()))

    def load_testopia(self, *packages):
        try:
            assert(self.testopia.testcases and not packages)
        except (AttributeError, AssertionError):
            self.testopia = Testopia(
                self.get_release()
              , packages or self.get_package_list()
            )

        return self.testopia

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

class NullTestReport(TestReport):
    _type = "No"

    def __init__(tr, config, log, _date = date, *a, **kw):
        super(NullTestReport, tr).__init__(config, log, _date, *a, **kw)
        tr.id = None
        tr.path = join(os.getcwd(), "None")

    def __bool__(tr):
        return False

    def __nonzero__(tr):
        '''python-2.x compat, see __bool__()'''
        return tr.__bool__()

    def target_wd(self, *paths):
        return join(self.config.target_tempdir, *paths)

    def _get_updater_id(tr):
        return None

    def _parser(tr):
        return None

class SwampTestReport(TestReport):
    _type = "SWAMP"

    def __init__(self, *a, **kw):
        super(SwampTestReport, self).__init__(*a, **kw)

        self._attrs += [
            'md5',
            'swampid',
            'patches',
        ]

    @property
    def id(self):
        return self.md5

    def _get_updater_id(self):
        return self.get_release()

    def _show_yourself_data(self):
        return [
            ('MD5SUM'  , self.md5),
            ('SWAMP ID', self.swampid),
        ] + super(SwampTestReport, self)._show_yourself_data()


    def _parser(self):
        return SWAMPMetadataParser()

    def download_source_rpm(self):
        download_source_rpm(
            self.repository,
            2
        )

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
        rel = self.get_release()
        if rel == '11':
            return '12'

        return rel

    def _parser(self):
        return OBSMetadataParser()

    def _show_yourself_data(self):
        return [
            ('ReviewRequestID'  , self.rrid),
            ('Rating'           , self.rating),
        ] + super(OBSTestReport, self)._show_yourself_data()

    def download_source_rpm(self):
        download_source_rpm(
            self.repository,
            3
        )
