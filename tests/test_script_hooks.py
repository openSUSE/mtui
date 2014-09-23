from nose.tools import eq_
from nose.tools import ok_
from temps import tmpdir
from os.path import join
from os.path import dirname
import os

from mtui.prompt import PreScript
from mtui.prompt import PostScript
from mtui.prompt import CompareScript
from mtui.template import SwampTestReport
from mtui.target import RunCommand
from mtui.target import TargetI
from mtui.target import Target
from mtui import messages

from .utils import TRF
from .utils import SF
from .utils import ConfigFake
from .utils import new_md5
from .utils import hostnames
from .utils import unused
from mtui.utils import unlines

scripts = [PreScript, PostScript, CompareScript]

def test_script_list():
    for x in scripts:
        yield check_script_list, x

def check_script_list(s):
    """
    Tests L{TestReport.script_hooks} returns expected script objects

    Fakes only TestReport template.
    """
    with tmpdir() as wdir:
        c = ConfigFake(overrides = dict(template_dir = wdir))
        tr = TRF(SwampTestReport, config = c)

        md5 = new_md5()
        tpl = join(wdir, md5)
        os.makedirs(tpl)
        tpl = join(tpl, "log")

        with open(tpl, 'w') as f:
            f.write("MD5SUM: {0}\n".format(md5))

        srcscripts = set([x for x in os.listdir("./scripts/" + s.subdir)])

        tr.read(tpl)
        scripts = tr.script_hooks(s).scripts
        ok_(len(scripts) > 0)
        for x in scripts:
            ok_(isinstance(x, s))
            ok_(x.name in srcscripts)
            srcscripts -= set([x.name])

class FileUploadFake:
    def __init__(self, targets, local_path, remote_path):
        pass

    def run(self):
        pass

class RunCommandFake(RunCommand):
    def run(self):
        pass

class TargetFake(TargetI):
    def __init__(self, hostname, lastout, lasterr):
        self.hostname = hostname
        self._lastout = lastout
        self._lasterr = lasterr

    def lastout(self):
        return self._lastout

    def lasterr(self):
        return self._lasterr

def test_run_remotes():
    for x in [PreScript, PostScript]:
        yield check_run_remotes, x

def check_run_remotes(s):
    """
    Tests `TestReport.script_hooks(s).run(ts)` results into
    "the script output" (by faked Target) written into result file

    Fakes
    1. the source scripts (copied to report workdir by TestReport

    2. the TestReport template

    3. Script upload and execution (as well as the Target)
    """
    with tmpdir() as wdir:
        sname = "script_x"
        scripts_src = join(wdir, "script_src")
        scripts_sub_src = join(scripts_src, s.subdir)
        os.makedirs(scripts_sub_src)
        with open(join(scripts_sub_src, sname), "w") as f:
            f.write("unused")

        c = ConfigFake(dict(template_dir = wdir))
        tr = TRF(
            SwampTestReport,
            config          = c,
            file_uploader   = FileUploadFake,
            cmd_runner      = RunCommandFake,
            scripts_src_dir = scripts_src
        )

        md5 = new_md5()
        tpl = join(wdir, md5)
        os.makedirs(tpl)
        tpl = join(tpl, "log")

        with open(tpl, 'w') as f:
            f.write("MD5SUM: {0}\n".format(md5))

        tr.read(tpl)

        stdout = "foo stdout\n"
        stderr = "bar stderr"

        target = TargetFake(hostnames.foo, stdout, stderr)
        ss = tr.script_hooks(s)
        ss.run([target])

        with open(ss.scripts[0].result_file(target), 'r') as f:
            eq_(f.readlines(), [stdout, stderr])

def test_compare_script():
    with tmpdir() as wdir:
        tr = TRF(
            SwampTestReport,
            config = ConfigFake(dict(template_dir = wdir)),
        )

        md5 = new_md5()
        tpl = join(wdir, md5, "log")
        os.makedirs(dirname(tpl))
        with open(tpl, 'w') as f:
            f.write("MD5SUM: {0}\n".format(md5))

        tr.read(tpl)

        script = tr.scripts_wd("compare", "compare_new_licenses.sh")
        s = SF(CompareScript, tr, script)
        eq_(s.path, script)

        t = Target(hostnames.foo, unused, connect=False)

        pre_f = s._pre_file(t)
        with open(pre_f, 'w') as f:
            f.write("foo")

        post_f = s._post_file(t)
        with open(post_f, 'w') as f:
            f.write("bar")

        s.run([t])

        from pprint import pprint
        pprint(s.path)
        s.log.pprint()

        warning = s.log.warnings[0]
        ok_(isinstance(warning, messages.CompareScriptFailed))
        eq_(warning.argv, [s.path, pre_f, post_f])
        eq_(warning.stdout, '')
        ok_('ERROR: found new rpm license texts' in warning.stderr)
        ok_('-foo' in warning.stderr)
        ok_('+bar' in warning.stderr)
        eq_(warning.rc, 1)
