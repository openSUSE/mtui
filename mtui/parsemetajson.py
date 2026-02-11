"""A parser for extracting metadata from a JSON object."""

from .types import RequestReviewID


class JSONParser:
    """A parser for extracting metadata from a JSON object."""

    @staticmethod
    def parse(results, data) -> None:
        """Parses a JSON object and extracts metadata.

        Args:
            results: An object to store the parsed results.
            data: The JSON object to parse.
        """
        for i in data.get("jira"):
            results.jira[i] = "Description not available"

        for i in data.get("bugs"):
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
        for prod, pkgvers in data.get("packages").items():
            pkgs = {pkg: ver for pkg, _, ver in (p.split() for p in pkgvers)}
            packages[prod] = pkgs
        results.repositories = frozenset(data.get("repositories", []))
        results.packages = packages
