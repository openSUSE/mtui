# -*- coding: utf-8 -*-

from nose.tools import assert_false, ok_, eq_, raises
from unittest import TestCase

from collections import namedtuple
from tempfile import mkdtemp, mkstemp
from os.path import join
from errno import ENOENT, EEXIST
import shutil
import os

from mtui.template import NullTestReport
from mtui.template import OBSTestReport
from mtui.template import _TemplateIOError
from mtui.utils import UnknownSystemError
from mtui.target import Target
from mtui.types.obs import RequestReviewID
from mtui import messages
from .utils import LogFake
from .utils import LogTestingWrap
from .utils import StringIO
from .utils import touch
from .utils import ConfigFake
from .utils import get_nonexistent_path
from .utils import unused
from .utils import TRF
from .utils import refhosts_fixtures


@raises(_TemplateIOError)
def test_TestReport__open_and_parse_raises_templateioerror():
    class TestableReport(OBSTestReport):

        def _parse(self, f):
            # NOTE: here we are abusing the fact that the try/except
            # wraps this function too, though it probably should not
            raise IOError(EEXIST, 'sterr')

    tr = TRF(TestableReport)
    path = get_nonexistent_path()

    tr._open_and_parse(path)


def test_TestReport__copy_scripts_dst_exists():
    class TestableReport(OBSTestReport):

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
    class TestableReport(OBSTestReport):

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
    class TestableReport(OBSTestReport):

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
        class TestableReport(OBSTestReport):
            t_copytree = []
            t_ensure_executable = []

            def _copytree(self, *args, **kw):
                self.t_copytree.append((args, kw))

            def _ensure_executable(self, pattern):
                self.t_ensure_executable.append(pattern)

        tr = TRF(TestableReport,
                 config=ConfigFake(dict(datadir='foodata'))
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
        tr = TRF(OBSTestReport)
        pattern = self.in_temp('/*')
        tr._ensure_executable(pattern)

        files = [(r, ds, fs) for r, ds, fs in os.walk(self.tmp_dir)]
        head = files.pop(0)
        eq_(files, [])
        eq_(head[0], self.tmp_dir)
        eq_(head[1], [])
        eq_(head[2], [])

    def test_ensure_executable_makes_executable(self):
        tr = TRF(OBSTestReport)

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

        tr = TRF(OBSTestReport)
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

        tr = TRF(OBSTestReport)
        try:
            tr._copytree(src, dst)
        except OSError as e:
            eq_(e.errno, EEXIST)
        else:
            ok_(False, "OSError expected")

    def test_copytree_src_missing(self):
        src = self.in_temp('src')
        dst = self.in_temp('dst')

        tr = TRF(OBSTestReport)
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

    tr = TRF(OBSTestReport)
    tr.systems = {'foo': 'bar', 'qux': 'quux'}
    tr.connect_targets(make_target=TargetFake)
    ts = tr.targets

    eq_(len(ts), 2)

    for (k, v), (h, t) in zip(list(tr.systems.items()), list(ts.items())):
        eq_(k, h)
        eq_(t.hostname, k)
        eq_(t.system, v)


def test_TestReport_refhosts_from_tp():
    """
    Test L{TestReport._refhosts_from_tp}
    """
    def check(case):
        tr = TRF(
            OBSTestReport,
            config=ConfigFake(
                overrides=dict(
                    refhosts_path=refhosts_fixtures['basic'],
                    refhosts_resolvers='path',
                    location='foolocation',
                    template_dir='footpldir')))

        tr._refhosts_from_tp(case.testplatform)
        eq_(set(case.hosts.keys()), set(tr.systems.keys()))
        eq_(
              LogTestingWrap(tr.log).all(), dict([(k, [v.format(**case._asdict()) for v in vs])
                                                  for k, vs in list(case.logs.items())
                                                  ])
            #    , case.name
        )

    Case = namedtuple('Case', ['name', 'testplatform', 'hosts', 'logs'])

    cases = [
        Case(
            'happy path', 'base=sles(major=11,minor=sp3);arch=[i386,x86_64]',
            {'fletcher.example.com': 'sles11sp3-x86_64',
             'cunningham.example.com': 'sles11sp3-i386'},
            LogTestingWrap().all()),
        Case(
            'failure to parse testplatform', 'unparsable testplatform', {},
            LogTestingWrap().warning(
                "failed to parse testplatform '{testplatform}'").error(
                'error when parsing line "{testplatform}"').all()),
        Case(
            'nothing found in refhosts',
            'base=sles(major=11,minor=sp3);arch=[ppc64]', {},
            LogTestingWrap().warning(
                "nothing found for testplatform '{testplatform}'").all())]

    for c in cases:
        yield check, c


def test_TestReportParse_parsed_testplatform():
    tr = TRF(OBSTestReport)

    tps = ['footp1', 'footp2']

    tpl_data = ["Testplatform: "+x for x in tps]
    tpl_data = "\n".join(tpl_data)
    tpl = StringIO(tpl_data)

    tr._parse(tpl)
    ok_(tr.testplatforms, tps)


def test_obs_get_testsuite_comment():
    tr = TRF(OBSTestReport)
    tr.rrid = RequestReviewID("SUSE:Maintenance:1:1")
    comment = tr.get_testsuite_comment("tsuite", "a horse")
    eq_(str(comment), "testing tsuite on OBS {0} on a horse".format(
        tr.rrid,
    ))


def test_NullTestReport():
    tr = NullTestReport(ConfigFake(), LogFake())
    assert_false(tr)
    eq_(tr.id, None)


def test_select():
    class TargetFake(Target):

        def __init__(self, *args, **kw):
            super(TargetFake, self).__init__(*args, connect=False, **kw)

        def add_history(self, comment):
            pass
    tr = NullTestReport(ConfigFake(), LogFake())
    tr.systems.update(
      foo='fubar',
      bar='snafu',
      qux='snafubar',
    )
    tr.connect_targets(make_target=TargetFake)
    ts = tr.targets
    ts['qux'].state = 'disabled'

    def s(s): return set(s.split())

    selected = set(ts.select().keys())
    eq_(selected, set('foo bar qux'.split()))
    selected = set(ts.select('bar qux'.split()).keys())
    eq_(selected, set('bar qux'.split()))
    selected = set(ts.select(enabled=True).keys())
    eq_(selected, set('foo bar'.split()))
    selected = set(ts.select(['qux'], enabled=True).keys())
    eq_(selected, set())


def test_get_release():
    cases = [] + [({'foo': x},
                   '12')
                  for x
                  in
                  ['sled12None-x86_64', 'sles12None-x86_64', 'sles12None-x86_64', 'cloud',
                   'slms', 'manager3']] + [({'foo': 'rhel'},
                                            'YUM')] + [({'foo': x},
                                                        '11')
                                                       for x
                                                       in
                                                       ['sled11sp3-i386', 'sled11sp3-x86_64', 'sles11sp3-i386',
                                                        'sles11sp3-s390x', 'sles11sp3-x86_64', 'manager2', 'studio',
                                                        'sles4vmware']]

    for system, result in cases:
        yield check_release, OBSTestReport, system, result


def check_release(report, systems, result):
    tr = TRF(report)
    tr.systems = systems
    eq_(tr.get_release(), result)


def test_get_release_exc():
    yield raises(UnknownSystemError)(check_release), OBSTestReport, {'foo': ''}, unused


def test_get_doers():
    t = TRF(OBSTestReport)
    t.get_release = lambda: unused

    cases = [
        (t.get_preparer, messages.MissingPreparerError),
        (t.get_updater, messages.MissingUpdaterError),
        (t.get_installer, messages.MissingInstallerError),
        (t.get_uninstaller, messages.MissingUninstallerError),
        (t.get_downgrader, messages.MissingDowngraderError),
    ]

    for fn, exc in cases:
        yield raises(exc)(fn)

    for fn, _ in cases:
        yield raises(messages.MissingDoerError)(fn)
