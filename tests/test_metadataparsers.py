from mtui.parsemeta import MetadataParser, ReducedMetadataParser
from mtui.parsemetajson import JSONParser
from mtui.types.obs import RequestReviewID


class FakeTestreport:
    def __init__(self):
        self.hostnames = set()
        self.bugs = {}
        self.jira = {}
        self.testplatforms = []
        self.category = ""
        self.packager = ""
        self.reviewer = ""
        self.repository = None
        self.packages = {}
        self.rrid = None
        self.rating = None


def test_parse_old(log_txt):
    report = FakeTestreport()

    for line in log_txt.splitlines():
        MetadataParser.parse(report, line)

    assert report.rating == "low"
    assert report.bugs == {"12345": "[foo] bar"}
    assert report.category == "recommended"
    assert report.rrid == RequestReviewID("SUSE:Maintenance:24993:275518")
    assert report.jira == {"SLE-22357": ""}
    assert report.repository == "http://download.suse.de/ibs/SUSE:/Maintenance:/24993/"
    assert report.reviewer == "#maintenance"
    assert report.packager == "slemke@suse.com"
    assert report.products == [
        "SLE-Module-Development-Tools-OBS 15-SP4 (aarch64, ppc64le, s390x, x86_64)",
        "SLE-Module-Python2 15-SP3 (aarch64, ppc64le, s390x, x86_64)",
    ]
    assert report.testplatforms == [
        "base=sles(major=15,minor=sp3);arch=[s390x,x86_64];addon=python2(major=15,minor=sp3)",
        "base=sles(major=15,minor=sp4);arch=[s390x,x86_64];addon=Development-Tools-OBS(major=15,minor=sp4)",
        "base=SLES(major=15,minor=SP3);arch=[aarch64,ppc64le,s390x,x86_64];addon=sle-module-python2(major=15,minor=SP3)",
        "base=SLES(major=15,minor=SP4);arch=[aarch64,ppc64le,s390x,x86_64];addon=sle-module-development-tools-obs(major=15,minor=SP4)",
    ]
    assert report.packages == {
        "15-SP3": {"sle-module-python2-release": "15.3-150300.59.4.1"},
        "15-SP4": {"sle-module-python2-release": "15.3-150300.59.4.1"},
        "default": {"sle-module-python2-release": "15.3-150300.59.4.1"},
    }
    assert report.hostnames == {"s390vsl138.suse.de", "s390vsl116.suse.de"}


def test_parse_new(log_txt, log_json):
    report = FakeTestreport()

    JSONParser.parse(report, log_json)

    for line in log_txt.splitlines():
        ReducedMetadataParser.parse(report, line)

    assert report.rating == "low"
    assert report.bugs == {"12345": "[foo] bar"}
    assert report.category == "recommended"
    assert report.rrid == RequestReviewID("SUSE:Maintenance:24993:275518")
    assert report.jira == {"SLE-22357": ""}
    assert report.repository == "http://download.suse.de/ibs/SUSE:/Maintenance:/24993/"
    assert report.reviewer == ""  # new format don't have this field
    assert report.packager == "slemke@suse.com"
    assert report.products == [
        "SLE-Module-Development-Tools-OBS 15-SP4 (aarch64, ppc64le, s390x, x86_64)",
        "SLE-Module-Python2 15-SP3 (aarch64, ppc64le, s390x, x86_64)",
    ]
    assert report.testplatforms == [
        "base=sles(major=15,minor=sp3);arch=[s390x,x86_64];addon=python2(major=15,minor=sp3)",
        "base=sles(major=15,minor=sp4);arch=[s390x,x86_64];addon=Development-Tools-OBS(major=15,minor=sp4)",
        "base=SLES(major=15,minor=SP3);arch=[aarch64,ppc64le,s390x,x86_64];addon=sle-module-python2(major=15,minor=SP3)",
        "base=SLES(major=15,minor=SP4);arch=[aarch64,ppc64le,s390x,x86_64];addon=sle-module-development-tools-obs(major=15,minor=SP4)",
    ]
    # new format hasn't separate packages field without product version ...
    assert report.packages == {
        "15-SP3": {"sle-module-python2-release": "15.3-150300.59.4.1"},
        "15-SP4": {"sle-module-python2-release": "15.3-150300.59.4.1"},
    }
    assert report.hostnames == {"s390vsl138.suse.de", "s390vsl116.suse.de"}
