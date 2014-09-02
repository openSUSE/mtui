from unittest import TestCase
from nose.tools import eq_
from mtui.template import SwampTestReport
from mtui.types.md5 import MD5Hash

from .utils import StringIO
from .utils import unused
from .test_template import TRF

class TestableSwampTestReport(SwampTestReport):
    def _open_and_parse(self, path):
        self._parse(self.t_tpl)

    def copy_scripts(self):
        pass

class TestTestReport_show_yourself(TestCase):
    def test_md5(self):
        uid = MD5Hash('0a27320617cd0d989b2ed1bcc5682c0f')
        data = {
            'Category': 'recommended',
            'Hosts': '',
            'Reviewer': 'leonardo',
            'Packager': 'lmb@linbit.com',
            'Bugs': '825657, 833764',
            'Packages': 'drbd drbd-bash-completion',
            'Testreport': 'http://qam.suse.de/testreports/{0}/log'.format(uid),
            'MD5SUM': uid,
            'SWAMP ID': '54755',
            'Build': 'http://hilbert.nue.suse.com/abuildstat/patchinfo/{0}'.format(uid),
            'SAT': '8438'
        }

        lines = [
            '',
            'Products: SLE-DEBUGINFO 11-SP3 '
                '(i386, ia64, ppc64, s390x, x86_64), SLE-HAE 11-SP3'
                ' (i386, ia64, ppc64, s390x, x86_64), SLE-RT 11-SP3'
                ' (x86_64)',
            'Category: ' + data['Category'],
            'SAT Patch No: 8438',
            'MD5 sum: {0}'.format(uid),
            'SUBSWAMPID: ' + data['SWAMP ID'],
            'Packager: ' + data['Packager'],
            'Bugs: 833764, 825657',
            'Packages: drbd >= 8.4.4-0.18.1, drbd-bash-completion >= 8.4.4-0.18.1',
            'SRCRPMs: drbd',
            'Test Plan Reviewers: ' + data['Reviewer'],
            'Testplatform: ' + ';'.join([
                'base=sles(major=11,minor=sp3)',
                'arch=[i386,ia64,ppc64,s390x,x86_64]',
                'addon=hae(major=11,minor=sp3)',
            ]),
            'Testplatform: ' + ';'.join([
                'base=sles(major=11,minor=sp3)',
                'arch=[x86_64]',
                'addon=rt(major=11,minor=sp3)',
            ]),
        ]

        tr = TRF(TestableSwampTestReport)
        tr.t_tpl = StringIO("\n".join(lines))
        tr.read(unused)

        s = StringIO()
        tr.show_yourself(s)

        exp = StringIO()
        tr._aligned_write(exp, data)
        eq_(s.getvalue(), exp.getvalue())
