import re

from .types.obs import RequestReviewID


class MetadataParser:
    def parse_line(self, results, line: str) -> None:
        match = re.search("Products: (.+)", line)
        if match:
            results.products = match.group(1).replace("), ", ")|").split("|")
            return

        match = re.search("Category: (.+)", line)
        if match:
            results.category = match.group(1)
            return

        match = re.search("Packager: (.+)", line)
        if match:
            results.packager = match.group(1)
            return

        match = re.search("Packages: (.+)", line)
        if match:
            pkgs = {
                pack.split()[0]: pack.split()[2] for pack in match.group(1).split(",")
            }
            results.packages["default"] = pkgs
            return

        match = re.search("PackageVer: (.+)", line)
        if match:
            ret = {}

            pkgver = (x.strip(" )").split("(") for x in match.group(1).split(";"))

            for prod in pkgver:
                ver = prod[0]
                pkgs = {p: v for p, _, v in (pv.split() for pv in prod[1].split(", "))}
                ret[ver] = pkgs

            results.packages.update(ret)
            return

        match = re.search("Test Plan Reviewer(?:s)?: (.+)", line)
        if match:
            results.reviewer = match.group(1)
            return

        match = re.search(r'Jira ([A-Z]+-\d+) \("(.*)"\):', line)
        if match:
            results.jira[match.group(1)] = match.group(2)
            return

        match = re.search(r'Bug (\d+) \("(.*)"\):', line)
        if match:
            results.bugs[match.group(1)] = match.group(2)
            return

        match = re.search("Testplatform: (.*)", line)
        if match:
            results.testplatforms.append(match.group(1))
            return

        match = re.search(r".* \(reference host: (\S+).*\)", line)
        if match:
            if "?" not in match.group(1):
                results.hostnames.add(match.group(1))
                return

        match = re.search("Bugs: (.*)", line)
        if match:
            for bug in match.group(1).split(","):
                results.bugs[bug.strip(" ")] = "Description not available"
            return

        match = re.search("Jira: (.*)", line)
        if match:
            for issue in match.group(1).split(","):
                results.jira[issue.strip(" ")] = "Description not available"
            return

        m = re.match("Repository: (.+)", line)
        if m:
            results.repository = m.group(1)
            return
        return


class OBSMetadataParser(MetadataParser):
    def parse_line(self, results, line: str) -> None:
        m = re.match("Rating: (.+)", line)
        if m:
            results.rating = m.group(1)
            return

        m = re.match("ReviewRequestID: (.+)", line)
        if m:
            results.rrid = RequestReviewID(m.group(1))
            return

        # continue with parernt parse
        return super().parse_line(results, line)
