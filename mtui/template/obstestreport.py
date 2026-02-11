"""A `TestReport` implementation for OBS test reports."""

from typing import final

from ..parsemeta import ReducedMetadataParser
from ..parsemetajson import JSONParser
from ..repoparse import obsrepoparse
from ..target import Target
from ..target.hostgroup import HostsGroup
from ..types import Product, RequestReviewID
from .testreport import TestReport


@final
class OBSTestReport(TestReport):
    """A `TestReport` implementation for OBS test reports."""

    def __init__(self, *a, **kw) -> None:
        """Initializes the `OBSTestReport` object."""
        super().__init__(*a, **kw)

        self.rrid: RequestReviewID
        self.rating = ""
        self.realid = ""

        self._attrs += ["rrid", "rating", "realid"]

    @property
    def _type(self) -> str:
        """Returns the type of the test report."""
        return "OBS"

    @property
    def id(self) -> str:
        """Returns the ID of the test report."""
        return str(self.rrid)

    def _parser(self):
        """Returns a dictionary of parsers for the test report."""
        parsers = {
            "hosts": ReducedMetadataParser,
            "json": JSONParser,
        }
        return parsers

    def _update_repos_parser(self) -> dict[Product, str]:
        """Returns a dictionary of update repositories."""
        return obsrepoparse(self.repository, self.report_wd())

    def _show_yourself_data(self) -> list[tuple[str, str]]:
        """Returns a list of data to be displayed by `list_metadata`."""
        return [
            ("ReviewRequestID", str(self.rrid)),
            ("Rating", self.rating),
            ("Real ID", self.realid),
        ] + super()._show_yourself_data()

    def set_repo(self, target: Target, operation: str) -> None:
        """Adds or removes a repository on a target host.

        Args:
            target: The target host.
            operation: The operation to perform ("add" or "remove").
        """
        if operation == "add":
            target.run_zypper("-n ar -ckn", self.update_repos, self.rrid)
        elif operation == "remove":
            target.run_zypper("-n rr", self.update_repos, self.rrid)
        else:
            raise ValueError("Not supported repose operation {}".format(operation))

    def list_update_commands(self, targets: HostsGroup, display) -> None:
        """Lists the update commands for the target hosts.

        Args:
            targets: The target hosts.
            display: The display function to use.
        """
        packages = self.get_package_list()
        repa = f":p={self.rrid.maintenance_id}:{self.rrid.review_id}"
        for hn, t in targets.items():
            display(
                f"{hn} - commands: \n{t.get_updater()['command'].safe_substitute(repa=repa, packages=packages)}"
            )

    def check_hash(self) -> bool:
        return True
