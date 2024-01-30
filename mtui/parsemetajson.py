from .types.obs import RequestReviewID


class JSONParser:
    @staticmethod
    def parse(results, data):
        for i in data.get("jira"):
            results.jira[i] = "Description not available"

        for i in data.get("bugs"):
            results.bugs[i] = "Description not available"

        results.rrid = RequestReviewID(data.get("rrid"))
        results.packager = data.get("packager")
        results.srpms = data.get("SRCRPMs")
        results.rating = data.get("rating")
        results.repository = data.get("repository")
        results.category = data.get("category")
        results.testplatforms = data.get("testplatform")
        results.products = data.get("products")

        packages = {}
        for prod, pkgvers in data.get("packages").items():
            pkgs = {pkg: ver for pkg, _, ver in (p.split() for p in pkgvers)}
            packages[prod] = pkgs
        results.packages = packages
