# -*- coding: utf-8 -*-

from nose.tools import ok_, eq_, raises
from unittest import TestCase

from collections import namedtuple
from tempfile import mkdtemp, mkstemp
from os.path import join
from errno import ENOENT, EPERM, EEXIST
import shutil
import os
from copy import deepcopy

from mtui.template import _TestReportFactory
from mtui.template import _TemplateIOError
from mtui.template import TestReport
from mtui.target import Target
from mtui.types import MD5Hash
from .utils import LogFake
from .utils import StringIO
from .utils import touch
from .utils import ConfigFake
from .utils import get_nonexistent_path

from traceback import format_exc

def test_instance_factory():
    from mtui.template import TestReportFactory
    ok_(isinstance(TestReportFactory, _TestReportFactory))

def test_TestReportFactory_no_md5():
    c = ConfigFake()
    c.template_dir = 'foodir'
    c.location = 'fooloc'
    f = _TestReportFactory()
    l = LogFake()
    tr = f(c, l)
    ok_(isinstance(tr, TestReport))
    eq_(tr.md5, None)
    eq_(tr.packages, {})
    eq_(tr.systems, {})
    eq_(tr.bugs, {})
    eq_(tr.testplatforms, [])
    eq_(tr.location, 'fooloc')
    eq_(tr.directory, 'foodir')
    ok_(tr.config is c)
    ok_(tr.log is l)
    eq_(l.debugs, ['TestReportFactory: not using template'])

def test_TestReportFactory_call_md5():
    class F(_TestReportFactory):
        _test_factory_md5_called = False

        def _factory_md5(self, config, log, tr, md5):
            self._test_factory_md5_called = True
            self.config = config
            self.log = log
            self.tr = tr
            self.md5 = md5

    c = ConfigFake()
    f = F()
    eq_(f._test_factory_md5_called, False)
    l = LogFake()
    md5 = MD5Hash('82407e2d7113cfde72f65d81e4ffee61')
    f(c, l, md5=md5)
    eq_(f._test_factory_md5_called, True)
    ok_(isinstance(f.tr, TestReport))
    ok_(f.config is c)
    ok_(f.log is l)
    eq_(f.md5, md5)

def TestReportMocker(read_fail=None, read_error=None):
    if not read_fail:
        read_fail = []

    if read_fail and read_error is None:
        raise ValueError('Invalid read_error {0!r}'.format(read_error))

    class TestReportMock(TestReport):
        read_cnt = 0

        def read(self, path):
            self.read_cnt += 1
            if self.read_cnt in read_fail:
                raise read_error()

    return TestReportMock

class TestReportFactoryMockFactoryMd5(_TestReportFactory):
    def __init__(self):
        super(TestReportFactoryMockFactoryMd5, self).__init__()
        self.t_ensure_dir = []
        self.t_svn_check = []
        self.t_counts = []

    def _ensure_dir_exists(self, path, on_create=None):
        ok_(callable(on_create))
        self.t_ensure_dir.append(path)

    def svn_checkout(self, path, uri):
        self.t_svn_check.append((path, uri))

    def _factory_md5(self, config, log, tr, md5, _count=0):
        self.t_counts.append(_count)
        return super(TestReportFactoryMockFactoryMd5, self)\
            ._factory_md5(config, log, tr, md5, _count)

def test_TestReportFactory_factory_md5_no_fail():
    c = ConfigFake()
    c.template_dir = '/tmp/foo'
    c.svn_path = 'svnpath'

    f = TestReportFactoryMockFactoryMd5()
    f.TestReport = TestReportMocker()
    l = LogFake()
    tr = f(c, l, md5=MD5Hash('82407e2d7113cfde72f65d81e4ffee61'))
    ok_(isinstance(tr, f.TestReport))
    eq_(f.t_counts, [0])
    eq_(f.t_ensure_dir, [])

def test_TestReportFactory_factory_md5_with_checkout():
    c = ConfigFake()
    c.template_dir = '/tmp/foo'
    c.svn_path = 'svnpath'

    f = TestReportFactoryMockFactoryMd5()
    f.TestReport = TestReportMocker(read_fail=[1],
        read_error=lambda: _TemplateIOError(ENOENT, ''))
    l = LogFake()
    md5 = MD5Hash('82407e2d7113cfde72f65d81e4ffee61')
    tr = f(c, l, md5=md5)
    ok_(isinstance(tr, f.TestReport))
    eq_(f.t_counts, [0, 1])
    eq_(f.t_svn_check, [(c.template_dir, join(c.svn_path, str(md5)))])
    eq_(f.t_ensure_dir, [c.template_dir])

def test_TestReportFactory_factory_md5_failing_checkout():
    c = ConfigFake()
    c.template_dir = '/tmp/foo'
    c.svn_path = 'svnpath'

    f = TestReportFactoryMockFactoryMd5()
    f.TestReport = TestReportMocker(read_fail=[1, 2, 3],
        read_error=lambda: _TemplateIOError(ENOENT, ''))
    l = LogFake()
    md5 = MD5Hash('82407e2d7113cfde72f65d81e4ffee61')
    try:
        f(c, l, md5=md5)
    except IOError:
        eq_(f.t_counts, [0, 1])
        eq_(f.t_svn_check, [(c.template_dir, join(c.svn_path, str(md5)))])
        eq_(f.t_ensure_dir, [c.template_dir])
    else:
        ok_(False)

def test_TestReportFactory_factory_md5_other_ioerror():
    c = ConfigFake()
    c.template_dir = '/tmp/foo'
    c.svn_path = 'svnpath'

    f = TestReportFactoryMockFactoryMd5()
    f.TestReport = TestReportMocker(read_fail=[1, 2, 3],
        read_error=lambda: IOError(EPERM, ''))
    l = LogFake()
    try:
        f(c, l, md5=MD5Hash('82407e2d7113cfde72f65d81e4ffee61'))
    except IOError:
        eq_(f.t_counts, [0])
        eq_(f.t_svn_check, [])
        eq_(f.t_ensure_dir, [])
    else:
        ok_(False)

def test_TestReportFactory_ensure_dir_exists():
    f = _TestReportFactory()
    d = '/tmp/mtui-unittestsuite-foobar'
    try:
        os.rmdir(d)
    except OSError as e:
        if e.errno != ENOENT:
            raise
    except:
        raise
    f._ensure_dir_exists(d)
    os.rmdir(d)

def test_TestReportFactory_double_ensure_dir_exists():
    """
    ensure_dir_exists is obviously supposed to be convergent so second
    call should result in the same state. This test asserts mainly that
    OSError(EEXIST) is not raised on second call.
    """
    f = _TestReportFactory()
    d = '/tmp/mtui-unittestsuite-foobar'
    try:
        os.rmdir(d)
    except OSError as e:
        if not e.errno == ENOENT:
            raise
    f._ensure_dir_exists(d)
    f._ensure_dir_exists(d)
    os.rmdir(d)

def test_TestReportFactory__copy_scripts_src_missing():
    """
    Test the behaviour of factory_md5 when ENOENT happens during
    TestReport.read (which is the same that is catched when the
    testreport md5 is not checked out
    """
    class TestableReport(TestReport):
        def _copytree(self, *args, **kw):
            raise IOError(ENOENT, 'strerr', args[0])

        def _open_and_parse(self, path):
            pass

    class TestableFactory(_TestReportFactory):
        def __init__(self, *a, **kw):
            super(TestableFactory, self).__init__(*a, **kw)
            self.TestReport = TestableReport
            self.svn_checkout = lambda *a: None
            self._ensure_template_dir_exists = lambda *a: None
            self.t_factory_calls = 0

        def _factory_md5(self, *args, **kw):
            self.t_factory_calls += 1
            return super(TestableFactory, self)._factory_md5(*args, **kw)

    l = LogFake()
    c = ConfigFake()
    trf = TestableFactory()

    try:
        trf(c, l, md5=MD5Hash('82407e2d7113cfde72f65d81e4ffee61'))
    except EnvironmentError as e:
        pass
    else:
        ok_(False)

    eq_(trf.t_factory_calls, 1)

@raises(_TemplateIOError)
def test_TestReport__open_and_parse_raises_templateioerror():
    class TestableReport(TestReport):
        def _parse(self, f):
            # NOTE: here we are abusing the fact that the try/except
            # wraps this function too, though it probably should not
            raise IOError(EEXIST, 'sterr')

    l = LogFake()
    c = ConfigFake()
    tr = TestableReport(c, l)
    path = get_nonexistent_path()

    tr._open_and_parse(path)

def test_TestReport__copy_scripts_dst_exists():
    class TestableReport(TestReport):
        def _copytree(self, *args, **kw):
            raise OSError(EEXIST, 'strerr', args[1])

    l = LogFake()
    c = ConfigFake()
    tr = TestableReport(c, l)
    tr._copy_scripts(None, 'foo', None)

    eq_(l.errors, [])
    eq_(l.warnings, [
        'Copy scripts None -> foo failed. reason:',
        "[Errno 17] strerr: 'foo'",
    ])

def test_TestReport__copy_scripts_src_missing():
    class TestableReport(TestReport):
        def _copytree(self, *args, **kw):
            raise OSError(ENOENT, 'strerr', args[0])

    l = LogFake()
    c = ConfigFake()
    tr = TestableReport(c, l)
    tr._copy_scripts('foo', None, None)

    eq_(l.errors, [
        'Copy scripts foo -> None failed. reason:',
        "[Errno 2] strerr: 'foo'",
        'copy scripts manually',
    ])
    eq_(l.warnings, [])


@raises(OSError)
def test_TestReport__copy_scripts_on_error():
    class TestableReport(TestReport):
        def _copytree(self, *args, **kw):
            raise OSError(1, 'strerr')

    l = LogFake()
    c = ConfigFake()
    tr = TestableReport(c, l)
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

        l = LogFake()
        c = ConfigFake()
        c.datadir = 'foodata'


        tr = TestableReport(c, l)
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
        l = LogFake()
        c = ConfigFake()
        tr = TestReport(c, l)
        pattern = self.in_temp('/*')
        tr._ensure_executable(pattern)

        files = [(r, ds, fs) for r, ds, fs in os.walk(self.tmp_dir)]
        head = files.pop(0)
        eq_(files, [])
        eq_(head[0], self.tmp_dir)
        eq_(head[1], [])
        eq_(head[2], [])

    def test_ensure_executable_makes_executable(self):
        l = LogFake()
        c = ConfigFake()
        tr = TestReport(c, l)

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

        l = LogFake()
        c = ConfigFake()
        tr = TestReport(c, l)
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

        l = LogFake()
        c = ConfigFake()
        tr = TestReport(c, l)
        try:
            tr._copytree(src, dst)
        except OSError as e:
            eq_(e.errno, EEXIST)
        else:
            ok_(False, "OSError expected")

    def test_copytree_src_missing(self):
        src = self.in_temp('src')
        dst = self.in_temp('dst')

        l = LogFake()
        c = ConfigFake()
        tr = TestReport(c, l)
        try:
            tr._copytree(src, dst)
        except OSError as e:
            eq_(e.errno, ENOENT)
        else:
            ok_(False, "OSError expected")

    def tearDown(self):
        print self.tmp_dir
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

    l = LogFake()
    c = ConfigFake()
    tr = TestReport(c, l)
    tr.targetFactory = TargetFake
    tr.systems = {'foo': 'bar', 'qux': 'quux'}
    ts = tr.connect_targets()

    for i in l.debugs:
        print i
    eq_(len(ts), 2)

    for (k, v), (h, t) in zip(tr.systems.items(), ts.items()):
        eq_(k, h)
        eq_(t.hostname, k)
        eq_(t.system, v)

def test_TestReport_load_systems_from_testplatforms():
    l = LogFake()
    c = ConfigFake()
    tr = TestReport(c, l)
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
    l = LogFake()
    c = RefhostFake.t_config()
    tr = TestReport(c, l)

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

    l = LogFake()
    c = RefhostFake.t_config()
    tr = TestReport(c, l)

    tr.refhostsFactory.refhosts_factory = RefhostFake_
    eq_(tr._refhosts_from_tp('footp'), {})
    eq_(l.warnings, ["failed to parse testplatform 'footp'"])

def test_TestReport_refhosts_from_tp_emptyresult():
    """
    Test L{TestReport._refhosts_from_tp} - nothing found in refhosts
    """
    class RefhostFake_(RefhostFake):
        def search(self):
            return []

    l = LogFake()
    c = RefhostFake.t_config()
    tr = TestReport(c, l)

    tr.refhostsFactory.refhosts_factory = RefhostFake_
    eq_(tr._refhosts_from_tp('footp'), {})
    eq_(l.warnings, ["nothing found for testplatform 'footp'"])

# {{{ template parser
def test_TestReportParse_parsed_md5():
    l = LogFake()
    c = ConfigFake()
    tr = TestReport(c, l)

    md5 = MD5Hash('8c60b7480fc521d7eeb322955b387165')

    tpl_data = [
        "SAT Patch No: 8655",
        "MD5 sum: {0}".format(md5),
        "SUBSWAMPID: 55446",
    ]
    tpl_data = "\n".join(tpl_data)
    tpl = StringIO(tpl_data)

    tr._parse(tpl)
    eq_(tr.md5, md5)

def test_TestReportParse_parsed_testplatform():
    l = LogFake()
    c = ConfigFake()
    tr = TestReport(c, l)

    tps = ['footp1', 'footp2']

    tpl_data = ["Testplatform: "+x for x in tps]
    tpl_data = "\n".join(tpl_data)
    tpl = StringIO(tpl_data)

    tr._parse(tpl)
    ok_(tr.testplatforms, tps)
# }}}
