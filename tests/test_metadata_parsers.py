"""Tests for ``mtui.test_reports.metadata_parsers``.

Merges the historical ``test_parsemeta.py``, ``test_parsemetajson.py``, and
``test_repoparse.py`` test files (one per legacy source module) into a
single suite that mirrors the consolidated source module.
"""

from pathlib import Path
from unittest.mock import MagicMock

from mtui.test_reports import metadata_parsers
from mtui.test_reports.metadata_parsers import JSONParser, ReducedMetadataParser
from mtui.types import Product, RequestReviewID

# ---------------------------------------------------------------------------
# Cross-format scenario (was tests/test_metadataparsers.py).
# ---------------------------------------------------------------------------


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
        self.products = []


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


# ---------------------------------------------------------------------------
# ReducedMetadataParser unit tests (was tests/test_parsemeta.py).
# ---------------------------------------------------------------------------


def test_reduced_metadata_parser_parse():
    """Test ReducedMetadataParser.parse."""
    results = MagicMock()
    results.hostnames = set()
    results.jira = {}
    results.bugs = {}

    # Test hostname parsing
    ReducedMetadataParser.parse(results, "some text (reference host: test_host)")
    assert "test_host" in results.hostnames

    # Test Jira issue parsing
    ReducedMetadataParser.parse(results, 'Jira ABC-123 ("Test Jira issue"):')
    assert results.jira["ABC-123"] == "Test Jira issue"

    # Test bug parsing
    ReducedMetadataParser.parse(results, 'Bug 123 ("Test bug"):')
    assert results.bugs["123"] == "Test bug"

    # Test Slack review marker parsing (written by set_slack_review)
    ReducedMetadataParser.parse(results, "Slack Review: C0123ABC/1700000000.000100")
    assert results.slack_review == ("C0123ABC", "1700000000.000100")


# ---------------------------------------------------------------------------
# JSONParser unit tests (was tests/test_parsemetajson.py).
# ---------------------------------------------------------------------------


def test_json_parser_parse():
    """Test JSONParser.parse."""
    results = MagicMock()
    results.jira = {}
    results.bugs = {}

    data = {
        "jira": ["ABC-123"],
        "bugs": ["123"],
        "rrid": "SUSE:Maintenance:1:1",
        "packager": "test_packager",
        "rating": "test_rating",
        "repository": "test_repository",
        "category": "test_category",
        "testplatform": ["test_platform"],
        "products": ["test_product"],
        "id": "test_id",
        "gitea_pr": "test_gitea_pr",
        "gitea_pr_api": "test_gitea_pr_api",
        "packages": {"test_prod": ["test_pkg 1.0 1.0"]},
        "repositories": ["test_repo"],
    }

    JSONParser.parse(results, data)

    assert results.jira["ABC-123"] == "Description not available"
    assert results.bugs["123"] == "Description not available"
    assert str(results.rrid) == "SUSE:Maintenance:1:1"
    assert results.packager == "test_packager"
    assert results.rating == "test_rating"
    assert results.repository == "test_repository"
    assert results.category == "test_category"
    assert results.testplatforms == ["test_platform"]
    assert results.products == ["test_product"]
    assert results.realid == "test_id"
    assert results.giteapr == "test_gitea_pr"
    assert results.giteaprapi == "test_gitea_pr_api"
    assert results.packages["test_prod"]["test_pkg"] == "1.0"
    assert results.repositories == frozenset(["test_repo"])


def test_json_parser_parse_tolerates_missing_optional_keys():
    """Absent/null jira, bugs and packages keys must not raise (use defaults)."""
    results = MagicMock()
    results.jira = {}
    results.bugs = {}

    # Minimal metadata: the list/dict-shaped keys are absent entirely, and one
    # is explicitly null. Previously `.get()` returned None and iterating /
    # `.items()` raised TypeError.
    data = {"rrid": "SUSE:Maintenance:1:1", "packages": None}

    JSONParser.parse(results, data)

    assert results.jira == {}
    assert results.bugs == {}
    assert results.packages == {}
    assert results.repositories == frozenset()


# ---------------------------------------------------------------------------
# *repoparse unit tests (was tests/test_repoparse.py).
# ---------------------------------------------------------------------------


def test_parse_product():
    """Test _parse_product."""
    products = metadata_parsers._parse_product("SLES 15 (x86_64, aarch64)")
    assert Product("SLES", "15", "x86_64") in products
    assert Product("SLES", "15", "aarch64") in products


def test_slrepoparse():
    """Test slrepoparse."""
    repos = metadata_parsers.slrepoparse("https://example.com", ["SLES 15 (x86_64)"])
    product = Product("SLES", "15", "x86_64")
    # we except repos has product key with exact value
    assert repos[product] == "https://example.com/images/repo/SLES-15-x86_64/"


def test_gitrepoparse():
    """Test gitrepoparse."""
    repos = metadata_parsers.gitrepoparse("https://example.com", ["SLES 15 (x86_64)"])
    product = Product("SLES", "15", "x86_64")
    # we except repos has product key with exact value
    assert repos[product] == "https://example.com/standard"


def test_reporepoparse():
    """Test reporepoparse."""
    repos = metadata_parsers.reporepoparse(
        frozenset(["https://example.com/SLES-15-x86_64/"]), ["SLES 15 (x86_64)"]
    )
    # we except repos has product key with exact value
    product = Product("SLES", "15", "x86_64")
    assert repos[product] == "https://example.com/SLES-15-x86_64/"


def test_obsrepoparse(tmpdir):
    """Test obsrepoparse."""
    project_xml = """
    <project>
      <repository name="SLE-15-x86_64">
        <path repository="update" project="SUSE:SLE-15:Update"/>
        <releasetarget project="SLE-Product-SLES:15:x86_64"/>
      </repository>
    </project>
    """
    path = Path(tmpdir)
    path.joinpath("project.xml").write_text(project_xml)

    repos = metadata_parsers.obsrepoparse("https://example.com", path)
    # we except repos has product key with exact value
    product = Product("SLES", "15", "x86_64")
    assert repos[product] == "https://example.com/SLE-15-x86_64"


def test_patchinfo_titles(tmp_path):
    """patchinfo_titles maps issue ids to their titles from patchinfo.xml."""
    (tmp_path / "patchinfo.xml").write_text(
        """<patchinfo>
          <issue tracker="bnc" id="1260938">Deprecate SHA1</issue>
          <issue tracker="bnc" id="1265607">All-Zero HMAC Key Detected</issue>
          <issue tracker="jsc" id="PED-1">A feature</issue>
        </patchinfo>"""
    )
    titles = metadata_parsers.patchinfo_titles(tmp_path)
    assert titles["1260938"] == "Deprecate SHA1"
    assert titles["1265607"] == "All-Zero HMAC Key Detected"
    assert titles["PED-1"] == "A feature"


def test_patchinfo_titles_absent(tmp_path):
    """No patchinfo.xml -> empty mapping (best effort, never raises)."""
    assert metadata_parsers.patchinfo_titles(tmp_path) == {}


def test_patchinfo_titles_malformed(tmp_path):
    """Unparseable patchinfo.xml -> empty mapping rather than an exception."""
    (tmp_path / "patchinfo.xml").write_text("<patchinfo><issue ")
    assert metadata_parsers.patchinfo_titles(tmp_path) == {}
