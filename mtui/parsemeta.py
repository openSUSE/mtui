import re

from .types.obs import RequestReviewID


class ReducedMetadataParser:
    hostnames = re.compile(r".* \(reference host: (\S+).*\)")
    jira = re.compile(r'Jira ([A-Z]+-\d+) \("(.*)"\):')
    bugs = re.compile(r'Bug (\d+) \("(.*)"\):')

    @classmethod
    def parse(cls, results, line: str) -> None:
        match = re.search(cls.hostnames, line)
        if match:
            if "?" not in match.group(1):
                results.hostnames.add(match.group(1))
                return

        match = re.search(cls.jira, line)
        if match:
            results.jira[match.group(1)] = match.group(2)
            return

        match = re.search(cls.bugs, line)
        if match:
            results.bugs[match.group(1)] = match.group(2)
            return

        return


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
        match = re.search(cls.products, line)
        if match:
            results.products = match.group(1).replace("), ", ")|").split("|")
            return

        match = re.search(cls.category, line)
        if match:
            results.category = match.group(1)
            return

        match = re.search(cls.packager, line)
        if match:
            results.packager = match.group(1)
            return

        match = re.search(cls.packages, line)
        if match:
            pkgs = {
                pack.split()[0]: pack.split()[2] for pack in match.group(1).split(",")
            }
            results.packages["default"] = pkgs
            return

        match = re.search(cls.pkgver, line)
        if match:
            ret = {}

            pkgver = (x.strip(" )").split("(") for x in match.group(1).split(";"))

            for prod in pkgver:
                ver = prod[0]
                pkgs = {p: v for p, _, v in (pv.split() for pv in prod[1].split(", "))}
                ret[ver] = pkgs

            results.packages.update(ret)
            return

        match = re.search(cls.reviewer, line)
        if match:
            results.reviewer = match.group(1)
            return

        match = re.search(cls.testplatforms, line)
        if match:
            results.testplatforms.append(match.group(1))
            return

        match = re.match(cls.repository, line)
        if match:
            results.repository = match.group(1)
            return

        match = re.match(cls.rating, line)
        if match:
            results.rating = match.group(1)
            return

        match = re.match(cls.rrid, line)
        if match:
            results.rrid = RequestReviewID(match.group(1))
            return

        match = re.search(cls.bugs2, line)
        if match:
            for bug in match.group(1).split(","):
                results.bugs[bug.strip(" ")] = "Description not available"
            return

        match = re.search(cls.jira2, line)
        if match:
            for issue in match.group(1).split(","):
                results.jira[issue.strip(" ")] = "Description not available"
            return

        # continue with parernt parse
        return ReducedMetadataParser.parse(results, line)
