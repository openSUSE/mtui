# -*- coding: utf-8 -*-

from nose.tools import ok_, eq_, raises
from unittest import TestCase

from collections import namedtuple
from tempfile import mkdtemp, mkstemp
from os.path import join
from os.path import dirname
from errno import EINTR, ENOENT, EPERM, EEXIST
import shutil
import os
from copy import deepcopy
from datetime import date

from mtui.template import _TemplateIOError
from mtui.template import TestReport
from mtui.template import SwampTestReport
from mtui.template import OBSTestReport
from mtui.template import TestsuiteComment
from mtui.template import SwampUpdateID
from mtui.template import _TemplateIOError
from mtui.template import QadbReportCommentLengthWarning
from mtui.template import UnknownSystemError
from mtui.target import Target
from mtui.types.md5 import MD5Hash
from mtui.types.obs import RequestReviewID
from .utils import LogFake
from .utils import StringIO
from .utils import touch
from .utils import ConfigFake
from .utils import get_nonexistent_path
from .utils import unused
from .utils import testreports

from traceback import format_exc

# FIXME: use temps python package to manage tempdirs/files

class TestReportSVNCheckoutFake(object):
    def __init__(self, template_path):
        self.path = template_path

    def __call__(self, *a, **kw):
        os.makedirs(dirname(self.path))
        with open(self.path, "w") as f:
            f.write("unused")

def TRF(tr, config=None, log=None, date_=None):
    if not config:
        config = ConfigFake()

    if not log:
        log = LogFake()

    if not date:
        date_ = date

    return tr(config, log, date_)

def test_UID_mtr_success():
    """
    Test UpdateID.make_testreport immediate success

    1. returns the TestReport instance from UpdateID.testreport_factory

    2. passes config and log objects on to the testreport instance
    """

    d = mkdtemp()

    try:
        c = ConfigFake()
        c.template_dir = d
        c.svn_path = unused

        u = SwampUpdateID('82407e2d7113cfde72f65d81e4ffee61')
        u.config = c
        u.log = LogFake()
        class TestReportFake(TestReport):
            def _parse(self, file_):
                pass

        u.testreport_factory = TestReportFake

        TestReportSVNCheckoutFake(u._template_path())()

        tr = u.make_testreport()

        ok_(isinstance(tr, TestReportFake))
        eq_(u.config, tr.config)
        eq_(u.log, tr.log)
    finally:
        shutil.rmtree(d)

def test_UID_mtr_with_checkout():
    """
    Test UpdateID.make_testreport does vcs_checkout if can't read the
    report
    """
    d = mkdtemp()

    try:
        c = ConfigFake()
        c.template_dir = d
        c.svn_path = 'svnpath'

        u = SwampUpdateID('82407e2d7113cfde72f65d81e4ffee61')
        u.config = c
        u.log = LogFake()
        u._vcs_checkout = TestReportSVNCheckoutFake(u._template_path())

        class TestReportFake(TestReport):
            def _parse(self, file_):
                pass
        u.testreport_factory = TestReportFake

        tr = u.make_testreport()

        ok_(isinstance(tr, TestReportFake))
        eq_(u.config, tr.config)
        eq_(u.log, tr.log)
    finally:
        shutil.rmtree(d)

def test_UID_mtr_failing_checkout():
    """
    Test UpdateID.make_testreport raises
    """
    d = mkdtemp()

    try:
        c = ConfigFake()
        c.template_dir = d
        c.svn_path = 'svnpath'

        u = SwampUpdateID('82407e2d7113cfde72f65d81e4ffee61')
        u.config = c
        u.log = LogFake()
        u._vcs_checkout = lambda *a, **kw: unused

        tr = u.make_testreport()
    except _TemplateIOError:
        pass
    else:
        ok_(False, "_TemplateIOError expected to be raised")
    finally:
        shutil.rmtree(d)

def test_UID_mtr_other_ioerror():
    """
    Test UpdateID.make_testreport raises
    """

    d = mkdtemp()

    try:
        c = ConfigFake()
        c.template_dir = d
        c.svn_path = 'svnpath'

        u = SwampUpdateID('82407e2d7113cfde72f65d81e4ffee61')
        u.config = c
        u.log = LogFake()
        u._vcs_checkout = lambda *a, **kw: ok_(False,
            "shouldn't try to perform checkout")

        TestReportSVNCheckoutFake(u._template_path())()
        os.chmod(u._template_path(), 0)

        try:
            tr = u.make_testreport()
        except _TemplateIOError as e:
            pass
        else:
            ok_(False, "_TemplateIOError expected to be raised")
    finally:
        shutil.rmtree(d)

def test_UID_mtr__copy_scripts_src_missing():
    """
    Test make_testreport() when scripts are missing in datadir
    """

    d = mkdtemp()

    try:
        c = ConfigFake()
        c.template_dir = d
        c.svn_path = 'svnpath'
        c.datadir = get_nonexistent_path()

        u = SwampUpdateID('82407e2d7113cfde72f65d81e4ffee61')
        u.config = c
        u.log = LogFake()
        u._vcs_checkout = TestReportSVNCheckoutFake(u._template_path())

        tr = u.make_testreport()
        eq_(tr.log.errors[-1], 'copy scripts manually')
        ok_(isinstance(tr, TestReport))
    finally:
        shutil.rmtree(d)

@raises(_TemplateIOError)
def test_TestReport__open_and_parse_raises_templateioerror():
    class TestableReport(TestReport):
        def _parse(self, f):
            # NOTE: here we are abusing the fact that the try/except
            # wraps this function too, though it probably should not
            raise IOError(EEXIST, 'sterr')

    tr = TRF(TestableReport)
    path = get_nonexistent_path()

    tr._open_and_parse(path)

def test_TestReport__copy_scripts_dst_exists():
    class TestableReport(TestReport):
        def _copytree(self, *args, **kw):
            raise OSError(EEXIST, 'strerr', args[1])

    tr = TRF(TestableReport)
    tr._copy_scripts(None, 'foo', None)

    eq_(tr.log.errors, [])
    eq_(tr.log.warnings, [
        'Copy scripts None -> foo failed. reason:',
        "[Errno 17] strerr: 'foo'",
    ])

def test_TestReport__copy_scripts_src_missing():
    class TestableReport(TestReport):
        def _copytree(self, *args, **kw):
            raise OSError(ENOENT, 'strerr', args[0])

    tr = TRF(TestableReport)
    tr._copy_scripts('foo', None, None)

    eq_(tr.log.errors, [
        'Copy scripts foo -> None failed. reason:',
        "[Errno 2] strerr: 'foo'",
        'copy scripts manually',
    ])
    eq_(tr.log.warnings, [])


@raises(OSError)
def test_TestReport__copy_scripts_on_error():
    class TestableReport(TestReport):
        def _copytree(self, *args, **kw):
            raise OSError(1, 'strerr')

    tr = TRF(TestableReport)
    tr._copy_scripts(None, None, None)

class TestTestReport_FileSystem_Hitters(TestCase):
    def setUp(self):
        self.tmp_dir = mkdtemp()

    def in_temp(self, suffix):
        return "{0}/{1}".format(self.tmp_dir, suffix)

    def test_copy_scripts(self):
        class TestableReport(TestReport):
            t_copytree = []
            t_ensure_executable = []
            def _copytree(self, *args, **kw):
                self.t_copytree.append((args, kw))
            def _ensure_executable(self, pattern):
                self.t_ensure_executable.append(pattern)

        tr = TRF(TestableReport,
            config = ConfigFake(dict(datadir = 'foodata'))
        )
        tr.path = join(self.tmp_dir, 'foopath')
        tr.copy_scripts()

        dst = "{0}/scripts".format(self.tmp_dir)

        ct_args, ct_kw = tr.t_copytree.pop()
        eq_(tr.t_copytree, [])
        eq_(ct_args[0], "foodata/scripts")
        eq_(ct_args[1], dst)
        ok_(ct_kw["ignore"])

        pattern = tr.t_ensure_executable.pop()
        eq_(tr.t_ensure_executable, [])
        eq_(pattern, "{0}/*/compare_*".format(dst))

    def test_ensure_executable_no_match(self):
        tr = TRF(TestReport)
        pattern = self.in_temp('/*')
        tr._ensure_executable(pattern)

        files = [(r, ds, fs) for r, ds, fs in os.walk(self.tmp_dir)]
        head = files.pop(0)
        eq_(files, [])
        eq_(head[0], self.tmp_dir)
        eq_(head[1], [])
        eq_(head[2], [])

    def test_ensure_executable_makes_executable(self):
        tr = TRF(TestReport)

        fd, f_txt = mkstemp(suffix='.txt', dir=self.tmp_dir)
        os.close(fd)
        fd, f_sh = mkstemp(suffix='.sh', dir=self.tmp_dir)
        os.close(fd)

        ok_(not os.access(f_txt, os.X_OK))
        ok_(not os.access(f_sh, os.X_OK))

        pattern = self.in_temp('*.sh')
        tr._ensure_executable(pattern)

        ok_(not os.access(f_txt, os.X_OK))
        ok_(os.access(f_sh, os.X_OK))

    def test_copytree_copies(self):
        src = self.in_temp('src')
        os.mkdir(src)
        dirs = [join(src, 'foo')]
        files = [
            join(src, 'quux'),
            join(dirs[0], 'bar'),
        ]

        for d in dirs:
            os.mkdir(d)
        for f in files:
            touch(f)

        with open(files[-1], 'a') as f:
            f.write('foo')

        dst = self.in_temp('dst')

        tr = TRF(TestReport)
        tr._copytree(src, dst)

        eq_(len(files), 2)
        for f in files:
            ok_(os.access(f, os.R_OK))

        f = open(files[-1].replace("src", "dst"), "r")
        eq_(f.read(), "foo")

    def test_copytree_dst_exists(self):
        src = self.in_temp('src')
        os.mkdir(src)

        dst = self.in_temp('dst')
        os.mkdir(dst)

        tr = TRF(TestReport)
        try:
            tr._copytree(src, dst)
        except OSError as e:
            eq_(e.errno, EEXIST)
        else:
            ok_(False, "OSError expected")

    def test_copytree_src_missing(self):
        src = self.in_temp('src')
        dst = self.in_temp('dst')

        tr = TRF(TestReport)
        try:
            tr._copytree(src, dst)
        except OSError as e:
            eq_(e.errno, ENOENT)
        else:
            ok_(False, "OSError expected")

    def tearDown(self):
        shutil.rmtree(self.tmp_dir)

def test_TestReport_connect_targets():
    class TargetFake(Target):
        def __init__(self, *args, **kw):
            ok_('connect' not in kw)
            kw['connect'] = False
            super(TargetFake, self).__init__(*args, **kw)
            self.t_history = []

        def add_history(self, comment):
            self.t_history.append(comment)

    tr = TRF(TestReport)
    tr.targetFactory = TargetFake
    tr.systems = {'foo': 'bar', 'qux': 'quux'}
    ts = tr.connect_targets()

    eq_(len(ts), 2)

    for (k, v), (h, t) in zip(tr.systems.items(), ts.items()):
        eq_(k, h)
        eq_(t.hostname, k)
        eq_(t.system, v)

def test_TestReport_load_systems_from_testplatforms():
    tr = TRF(TestReport)
    tps = ['t1', 't2']
    tr.testplatforms = deepcopy(tps)
    tr._refhosts_from_tp = lambda x: dict([(x, x+"val")])

    eq_(tr._refhosts_from_tp('f'), {'f': 'fval'})
    tr.systems = {'foo': 'bar'}
    tr.load_systems_from_testplatforms()

    eq_(len(tr.systems), 3)
    del tr.systems['foo']
    for i in tps:
        eq_(tr.systems[i], i+"val")

class RefhostFake:
    def __init__(self, xml, location):
        eq_(xml, 'fooxml')
        eq_(location, 'foolocation')

    def set_attributes_from_testplatform(self, x):
        self.t_tp = x

    def search(self):
        return [self.t_tp]

    def get_host_systemname(self, x):
        return x+"_system"

    @staticmethod
    def t_config():
        c = ConfigFake()
        c.refhosts_path = 'fooxml'
        c.refhosts_resolvers = 'path'
        c.location = 'foolocation'
        c.template_dir = 'footpldir'

        return c

def test_TestReport_refhosts_from_tp():
    """
    Test L{TestReport._refhosts_from_tp} - happy path
    """
    tr = TRF(TestReport, config = RefhostFake.t_config())

    tr.refhostsFactory.refhosts_factory = RefhostFake
    eq_(tr._refhosts_from_tp('foo'), {'foo': 'foo_system'})

def test_TestReport_refhosts_from_tp_ValueError():
    """
    Test L{TestReport._refhosts_from_tp} - failure while setting
    attributes
    """
    class RefhostFake_(RefhostFake):
        def set_attributes_from_testplatform(self, x):
            raise ValueError(x)

    tr = TRF(TestReport, config = RefhostFake.t_config())

    tr.refhostsFactory.refhosts_factory = RefhostFake_
    eq_(tr._refhosts_from_tp('footp'), {})
    eq_(tr.log.warnings, ["failed to parse testplatform 'footp'"])

def test_TestReport_refhosts_from_tp_emptyresult():
    """
    Test L{TestReport._refhosts_from_tp} - nothing found in refhosts
    """
    class RefhostFake_(RefhostFake):
        def search(self):
            return []

    tr = TRF(TestReport, config = RefhostFake.t_config())

    tr.refhostsFactory.refhosts_factory = RefhostFake_
    eq_(tr._refhosts_from_tp('footp'), {})
    eq_(tr.log.warnings, ["nothing found for testplatform 'footp'"])

# {{{ template parser
def test_TestReportParse_parsed_md5():
    tr = TRF(SwampTestReport)

    md5 = SwampUpdateID('8c60b7480fc521d7eeb322955b387165')

    tpl_data = [
        "SAT Patch No: 8655",
        "MD5 sum: {0}".format(md5.id),
        "SUBSWAMPID: 55446",
    ]
    tpl_data = "\n".join(tpl_data)
    tpl = StringIO(tpl_data)

    tr._parse(tpl)
    eq_(tr.md5, md5.id)

def test_TestReportParse_parsed_testplatform():
    tr = TRF(SwampTestReport)

    tps = ['footp1', 'footp2']

    tpl_data = ["Testplatform: "+x for x in tps]
    tpl_data = "\n".join(tpl_data)
    tpl = StringIO(tpl_data)

    tr._parse(tpl)
    ok_(tr.testplatforms, tps)
# }}}

def test_swamp_get_testsuite_comment():
    tr = TRF(SwampTestReport, date_ = date)
    tr.md5 = MD5Hash('8c60b7480fc521d7eeb322955b387165')
    comment = tr.get_testsuite_comment("tsuite")
    ok_(isinstance(comment, TestsuiteComment))
    eq_(str(comment), "testing tsuite on SWAMP {0} on {1}".format(
        tr.md5,
        date.today().strftime("%d/%m/%y"),
    ))

def test_obs_get_testsuite_comment():
    tr = TRF(OBSTestReport, date_ = date)
    tr.rrid = RequestReviewID("SUSE:Maintenance:1:1")
    comment = tr.get_testsuite_comment("tsuite")
    ok_(isinstance(comment, TestsuiteComment))
    eq_(str(comment), "testing tsuite on OBS {0} on {1}".format(
        tr.rrid,
        date.today().strftime("%d/%m/%y"),
    ))

def test_TC_edit_text():
    """
    Test L{TestsuiteComment.__str__} returns the new string after
    L{TestsuiteComment.edit_text} was executed.
    """
    tc = TestsuiteComment(LogFake(), "foo", "bar", date.today(),
        text_editor = lambda _: "meh"
    )
    ok_(str(tc).startswith("testing bar on foo on "))
    tc.edit_text()
    eq_(str(tc), "meh")

def test_TC_str_warning():
    """
    Test L{TestsuiteComment.__str__} issues the warning when comment too
    long
    """
    tc = TestsuiteComment(LogFake(), unused, unused, date.today(),
        lambda _: "-" * TestsuiteComment._max_comment_len
    )

    tc.edit_text()
    str(tc)
    eq_(tc.log.warnings, [])

    tc = TestsuiteComment(LogFake(), unused, unused, date.today(),
        lambda _: "-" * (TestsuiteComment._max_comment_len+1)
    )
    tc.edit_text()
    eq_(tc.log.warnings, [])
    str(tc)
    eq_(tc.log.warnings.pop(), QadbReportCommentLengthWarning())

def test_get_release():
    cases = [
        ] + [
            ({'foo': x},'12') for x in
            [
                'sled12None-x86_64',
                'sles12None-x86_64',
                'sles12None-x86_64',
            ]
        ] + [
            ({'foo': x}, '11') for x in
            [
                'sled11sp3-i386',
                'sled11sp3-x86_64',
                'sles11sp3-i386',
                'sles11sp3-s390x',
                'sles11sp3-x86_64',
                'mgr',
                'sles4vmware',
                'cloud',
                'studio',
                'slms',
                'manager',
            ]
        ] + [({'foo': 'rhel'}, 'YUM')]

    for r in testreports():
        for system, result in cases:
            yield check_release, r, system, result

def check_release(report, systems, result):
    tr = TRF(report)
    tr.systems = systems
    eq_(tr.get_release(), result)

def test_get_release_exc():
    for r in testreports():
        yield raises(UnknownSystemError)(check_release), r, {'foo': ''}, unused
