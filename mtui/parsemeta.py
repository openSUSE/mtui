import re
from typing import final

from .types import RequestReviewID


@final
class ReducedMetadataParser:
    hostnames = re.compile(r".* \(reference host: (\S+).*\)")
    jira = re.compile(r'Jira ([A-Z]+-\d+) \("(.*)"\):')
    bugs = re.compile(r'Bug (\d+) \("(.*)"\):')

    @classmethod
    def parse(cls, results, line: str) -> None:
        if match := re.search(cls.hostnames, line):
            if "?" not in match.group(1):
                results.hostnames.add(match.group(1))
                return

        if match := re.search(cls.jira, line):
            results.jira[match.group(1)] = match.group(2)
            return

        if match := re.search(cls.bugs, line):
            results.bugs[match.group(1)] = match.group(2)


@final
class MetadataParser:
    products = re.compile(r"Products: (.+)")
    category = re.compile(r"Category: (.+)")
    packager = re.compile(r"Packager: (.+)")
    packages = re.compile(r"Packages: (.+)")
    pkgver = re.compile(r"PackageVer: (.+)")
    reviewer = re.compile(r"Test Plan Reviewer(?:s)?: (.+)")
    testplatforms = re.compile(r"Testplatform: (.*)")
    repository = re.compile(r"Repository: (.+)")
    rrid = re.compile(r"ReviewRequestID: (.+)")
    rating = re.compile(r"Rating: (.+)")
    bugs2 = re.compile(r"Bugs: (.*)")
    jira2 = re.compile(r"Jira: (.*)")

    @classmethod
    def parse(cls, results, line: str) -> None:
        if match := re.search(cls.products, line):
            results.products = match.group(1).replace("), ", ")|").split("|")
            return

        if match := re.search(cls.category, line):
            results.category = match.group(1)
            return

        if match := re.search(cls.packager, line):
            results.packager = match.group(1)
            return

        if match := re.search(cls.packages, line):
            pkgs = {
                pack.split()[0]: pack.split()[2] for pack in match.group(1).split(",")
            }
            results.packages["default"] = pkgs
            return

        if match := re.search(cls.pkgver, line):
            ret = {}

            pkgver = (x.strip(" )").split("(") for x in match.group(1).split(";"))

            for prod in pkgver:
                ver = prod[0]
                pkgs = {p: v for p, _, v in (pv.split() for pv in prod[1].split(", "))}
                ret[ver] = pkgs

            results.packages.update(ret)
            return

        if match := re.search(cls.reviewer, line):
            results.reviewer = match.group(1)
            return

        if match := re.search(cls.testplatforms, line):
            results.testplatforms.append(match.group(1))
            return

        if match := re.match(cls.repository, line):
            results.repository = match.group(1)
            return

        if match := re.match(cls.rating, line):
            results.rating = match.group(1)
            return

        if match := re.match(cls.rrid, line):
            results.rrid = RequestReviewID(match.group(1))
            return

        if match := re.search(cls.bugs2, line):
            for bug in match.group(1).split(","):
                results.bugs[bug.strip(" ")] = "Description not available"
            return

        if match := re.search(cls.jira2, line):
            for issue in match.group(1).split(","):
                results.jira[issue.strip(" ")] = "Description not available"
            return

        # continue with parernt parse
        ReducedMetadataParser.parse(results, line)
