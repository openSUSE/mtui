from nose.tools import eq_
from nose.tools import ok_
from temps import tmpdir
from os.path import join
import os

from mtui.prompt import PreScript
from mtui.prompt import PostScript
from mtui.prompt import CompareScript
from mtui.template import SwampTestReport

from .utils import TRF
from .utils import ConfigFake
from .utils import new_md5

scripts = [PreScript, PostScript, CompareScript]

def test_script_list():
    for x in scripts:
        yield check_script_list, x

def check_script_list(s):
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
