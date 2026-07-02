"""Parsers that extract metadata from testreport sources.

Consolidates three historically separate modules:

* :class:`ReducedMetadataParser` — line-based parser for log/text metadata
  (formerly ``mtui.parsemeta``).
* :class:`JSONParser` — extracts metadata from the JSON envelope produced
  by the newer build pipeline (formerly ``mtui.parsemetajson``).
* The ``*repoparse`` helpers — derive a ``Product`` → repository-URL mapping
  from the various sources MTUI knows about (OBS, SUSE Linux, git, plain
  repositories). Formerly ``mtui.repoparse``.

These three parsers are imported together by every concrete
``TestReport`` subclass; merging them puts the parsing strategies next to
the report classes that consume them.
"""

import re
import xml.etree.ElementTree as ET
from itertools import chain
from os.path import join
from pathlib import Path
from typing import final

from ..types import Product, RequestReviewID
from .products import normalize, normalize_16

# ---------------------------------------------------------------------------
# Text/line-based metadata parser (was mtui/parsemeta.py).
# ---------------------------------------------------------------------------


@final
class ReducedMetadataParser:
    """A parser for extracting a reduced set of metadata from text."""

    hostnames = re.compile(r".* \(reference host: (\S+).*\)")
    jira = re.compile(r'Jira ([A-Z]+-\d+) \("(.*)"\):')
    bugs = re.compile(r'Bug (\d+) \("(.*)"\):')
    # "Slack Review: <channel>/<ts>" marker written by set_slack_review; parsed
    # back so metadata.slack_review is populated after a fresh checkout.
    slack_review = re.compile(r"Slack Review:\s*(\S+)/(\S+)\s*$")

    @classmethod
    def parse(cls, results, line: str) -> None:
        """Parses a line of text and extracts metadata.

        Args:
            results: An object to store the parsed results.
            line: The line of text to parse.

        """
        if (match := re.search(cls.hostnames, line)) and "?" not in match.group(1):
            results.hostnames.add(match.group(1))
            return

        if match := re.search(cls.jira, line):
            results.jira[match.group(1)] = match.group(2)
            return

        if match := re.search(cls.bugs, line):
            results.bugs[match.group(1)] = match.group(2)
            return

        if match := re.search(cls.slack_review, line):
            results.slack_review = (match.group(1), match.group(2))


# ---------------------------------------------------------------------------
# JSON envelope metadata parser (was mtui/parsemetajson.py).
# ---------------------------------------------------------------------------


class JSONParser:
    """A parser for extracting metadata from a JSON object."""

    @staticmethod
    def parse(results, data) -> None:
        """Parses a JSON object and extracts metadata.

        Args:
            results: An object to store the parsed results.
            data: The JSON object to parse.

        """
        for i in data.get("jira") or []:
            results.jira[i] = "Description not available"

        for i in data.get("bugs") or []:
            results.bugs[i] = "Description not available"

        results.rrid = RequestReviewID(data.get("rrid"))
        results.packager = data.get("packager")
        results.rating = data.get("rating")
        results.repository = data.get("repository")
        results.category = data.get("category")
        results.testplatforms = data.get("testplatform")
        results.products = data.get("products")
        results.realid = data.get("id")
        results.giteapr = data.get("gitea_pr")
        results.giteaprapi = data.get("gitea_pr_api")
        results.giteacohash = data.get("gitea_commit_hash")

        packages = {}
        for prod, pkgvers in (data.get("packages") or {}).items():
            pkgs = {pkg: ver for pkg, _, ver in (p.split() for p in pkgvers)}
            packages[prod] = pkgs
        results.repositories = frozenset(data.get("repositories", []))
        results.packages = packages


def patchinfo_titles(directory: Path) -> dict[str, str]:
    """Map issue id -> title from a checkout's ``patchinfo.xml``.

    The JSON metadata envelope only carries bare bug/jira *ids*, so
    :class:`JSONParser` fills their descriptions with a placeholder. The
    human-readable titles do exist in the checkout's ``patchinfo.xml``
    (the same source the server uses to build the report's ``BUGS SUMMARY``),
    as ``<issue tracker="bnc" id="123">title</issue>`` elements. This reads
    them so callers can enrich the ids with real titles.

    Best-effort: a missing or unparseable ``patchinfo.xml`` yields ``{}`` —
    not every report kind ships one, and a malformed file must never break
    loading.

    Args:
        directory: The checkout directory (where ``patchinfo.xml`` lives).

    Returns:
        A mapping of issue id to its title, empty when none are available.

    """
    pi = directory / "patchinfo.xml"
    if not pi.is_file():
        return {}
    try:
        root = ET.fromstring(pi.read_text(errors="replace"))
    except ET.ParseError:
        return {}
    titles: dict[str, str] = {}
    for issue in root.findall("issue"):
        iid = (issue.get("id") or "").strip()
        title = (issue.text or "").strip()
        if iid and title:
            titles[iid] = title
    return titles


# ---------------------------------------------------------------------------
# Repository-information parsers (was mtui/repoparse.py).
# ---------------------------------------------------------------------------


def _read_project(path: Path) -> ET.Element:
    """Reads and parses a `project.xml` file.

    Args:
        path: The path to the directory containing the `project.xml` file.

    Returns:
        An XML element representing the project.

    """
    xml = path.joinpath("project.xml").read_text()
    return ET.fromstringlist(xml)


def _xmlparse(xml):
    """Parses the XML element to extract repository information.

    Args:
        xml: The XML element to parse.

    Returns:
        A generator of repository information.

    """
    return (
        (x.find("releasetarget").attrib["project"].split(":")[-3:], x.attrib["name"])
        for x in xml.findall("repository/path[@repository='update']/..")
        if "DEBUG" not in x.attrib["name"]
    )


def obsrepoparse(repository: str, path: Path) -> dict[Product, str]:
    """Parses OBS repository information.

    Args:
        repository: The base repository URL.
        path: The path to the directory containing the `project.xml` file.

    Returns:
        A dictionary mapping `Product` objects to repository URLs.

    """
    project = _xmlparse(_read_project(path))
    return {
        Product(x[0], x[1], x[2]): join(repository, y)
        for x, y in map(normalize, project)
    }


def _parse_product(product: str) -> list[Product]:
    """Parses a product string into a list of `Product` objects.

    Args:
        product: The product string to parse.

    Returns:
        A list of `Product` objects.

    """
    b, a = product.split(" (")
    arch: list[str] = a.rstrip(")").split(", ")
    base: list[str] = b.split(" ")
    return [Product(base[0], base[1], x) for x in arch]


def slrepoparse(repository: str, products: list[str]) -> dict[Product, str]:
    """Parses SUSE Linux repository information.

    Args:
        repository: The base repository URL.
        products: A list of product strings.

    Returns:
        A dictionary mapping `Product` objects to repository URLs.

    """
    return {
        x: join(repository, "images/repo", f"{x.name}-{x.version}-{x.arch}/")
        for x in chain.from_iterable(_parse_product(pd) for pd in products)
    }


def gitrepoparse(repository: str, products: list[str]) -> dict[Product, str]:
    """Parses Git repository information.

    Args:
        repository: The base repository URL.
        products: A list of product strings.

    Returns:
        A dictionary mapping `Product` objects to repository URLs.

    """
    return {
        x: join(repository, "standard")
        for x in chain.from_iterable(_parse_product(pd) for pd in products)
    }


def reporepoparse(
    repositories: frozenset[str], products: list[str]
) -> dict[Product, str]:
    """Parses repository information from a set of repositories.

    Args:
        repositories: A set of repository URLs.
        products: A list of product strings.

    Returns:
        A dictionary mapping `Product` objects to repository URLs.

    """
    return {
        normalize_16(ps): repo
        for pd in products
        for ps in _parse_product(pd)
        for repo in repositories
        if f"{ps.name}-{ps.version}-{ps.arch}" in repo
    }
