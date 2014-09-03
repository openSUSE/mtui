from unittest import TestCase
from nose.tools import eq_

from mtui.template import SwampTestReport
from mtui.template import OBSTestReport
from mtui.types.md5 import MD5Hash
from mtui.types.obs import RequestReviewID

from .utils import StringIO
from .utils import unused
from .test_template import TRF

class TestableSwampTestReport(SwampTestReport):
    def _open_and_parse(self, path):
        self._parse(self.t_tpl)

    def copy_scripts(self):
        pass

class TestableOBSTestReport(OBSTestReport):
    def _open_and_parse(self, path):
        self._parse(self.t_tpl)

    def copy_scripts(self):
        pass


class TestTestReport_show_yourself(TestCase):
    def common_data(self, uid):
        return {
            'Category': 'recommended',
            'Hosts': '',
            'Reviewer': 'leonardo',
            'Packager': 'lmb@linbit.com',
            'Bugs': '825657, 833764',
            'Packages': 'drbd drbd-bash-completion',
            'Testreport': 'http://qam.suse.de/testreports/{0}/log'.format(uid),
        }

    def test_md5(self):
        uid = MD5Hash('0a27320617cd0d989b2ed1bcc5682c0f')
        data = self.common_data(uid)
        data.update({
            'MD5SUM': uid,
            'SWAMP ID': '54755',
            'Build': 'http://hilbert.nue.suse.com/abuildstat/patchinfo/{0}'.format(uid),
            'SAT': '8438',
            'Build': 'http://hilbert.nue.suse.com/abuildstat/patchinfo/{0}'.format(uid),
        })

        lines = [
            '',
            'Products: SLE-DEBUGINFO 11-SP3 '
                '(i386, ia64, ppc64, s390x, x86_64), SLE-HAE 11-SP3'
                ' (i386, ia64, ppc64, s390x, x86_64), SLE-RT 11-SP3'
                ' (x86_64)',
            'Category: {Category}',
            'SAT Patch No: 8438',
            'MD5 sum: {MD5SUM}',
            'SUBSWAMPID: {SWAMP ID}',
            'Packager: {Packager}',
            'Bugs: {Bugs}',
            'Packages: drbd >= 8.4.4-0.18.1, drbd-bash-completion >= 8.4.4-0.18.1',
            'SRCRPMs: drbd',
            'Test Plan Reviewers: {Reviewer}',
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

        self._run(TestableSwampTestReport, lines, data)

    def _run(self, report, lines_in, expected):
        tr = TRF(report)
        tr.t_tpl = StringIO("\n".join(lines_in).format(**expected))
        tr.read(unused)

        s = StringIO()
        tr.show_yourself(s)

        out = StringIO()
        tr._aligned_write(out, expected)
        eq_(s.getvalue(), out.getvalue())

    def test_rrid(self):
        uid = RequestReviewID("SUSE:Maintenance:32:36609")

        data = self.common_data(uid)
        data.update({
            'ReviewRequestID': str(uid),
            'Repository': 'http://download.suse.de/ibs/SUSE:/Maintenance:/32/',
            'Rating': 'moderate',
        })

        lines = [
            'Products: SLE-SERVER 12 (x86_64, s390x, ppc64le), SLE-DESKTOP 12 (x86_64)',
            'Category: {Category}',
            'Rating: {Rating}',
            'Packager: {Packager}',
            'Bugs: {Bugs}',
            'ReviewRequestID: {ReviewRequestID}',
            'Repository: {Repository}',
            'Packages: drbd >= 8.4.4-0.18.1, drbd-bash-completion >= 8.4.4-0.18.1',
            'SRCRPMs: drbd',
            'Suggested Test Plan Reviewers: {Reviewer}',
            'Testplatform: base=sles(major=12,minor=);arch=[s390x,x86_64]',
            'Testplatform: base=sled(major=12,minor=);arch=[x86_64]',
        ]

        self._run(TestableOBSTestReport, lines, data)

def check_parse_reviewer(report, input_, reviewer):
    tr = TRF(report)
    tpl = StringIO(input_.format(reviewer))

    tr._parse(tpl)
    eq_(tr.reviewer, reviewer)

def test_TR_parse_reviewer():
    inputs = [
        ("suggested_singular", "Suggested Test Plan Reviewer: {0}"),
        ("suggested_plural", "Suggested Test Plan Reviewers: {0}"),
        ("assigned_singular", "Test Plan Reviewer: {0}"),
        ("assigned_singular", "Test Plan Reviewers: {0}"),
    ]
    reports = [SwampTestReport, OBSTestReport]
    reviewer = "foobar"
    for r in reports:
        for n,i in inputs:
            yield check_parse_reviewer, r, i.format(reviewer), reviewer
