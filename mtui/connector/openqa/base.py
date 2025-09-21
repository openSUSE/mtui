"""The base class for all openQA connectors in mtui."""

from abc import ABC, abstractmethod
from logging import getLogger
from typing import ClassVar, Self

from openqa_client.client import OpenQA_Client as oqa
import openqa_client.exceptions

from .. import SMELT
from ...types import RequestReviewID, Test, URLs

logger = getLogger("mtui.connector.openqa")


class OpenQA(ABC):
    """An abstract base class for all openQA connectors in mtui."""

    kind: ClassVar[str] = "base"

    def __init__(self, config, host, smelt: SMELT, rrid: RequestReviewID) -> None:
        """Initializes the openQA connector.

        Args:
            config: The application configuration.
            host: The openQA instance host.
            smelt: The SMELT connector instance.
            rrid: The RequestReviewID of the current update.
        """
        logger.debug("init openQA client")
        self.host = host
        self.config = config
        self.smelt = smelt
        self.params: dict[str, str | int] = {}
        self.params["distri"] = config.openqa_install_distri
        self.params["scope"] = "relevant"
        self.params["latest"] = 1
        self.params["build"] = f":{rrid.maintenance_id}:{smelt.get_incident_name()}"
        self.client = oqa(host)
        self.pp: list[str] = []
        self.results: list[URLs] | list[Test] | None = None

    def _get_jobs(self):
        """Gets jobs from the openQA instance.

        Returns:
            A list of jobs, or None if the request fails.
        """
        logger.debug(f"Get data from openQA - {self.host}")

        try:
            jobs = self.client.openqa_request("GET", "jobs", self.params)["jobs"]
        except openqa_client.exceptions.RequestError as e:
            logger.debug("Openqa returned code: {!s}".format(e.args[2]))
            return None
        except openqa_client.exceptions.ConnectionError as e:
            logger.error(f"Cannont connect to openQA - {self.host}")
            logger.debug(f"openqa_client returned: {e}")
            return None

        return jobs

    @abstractmethod
    def _pretty_print(self, *args) -> list[str]:
        """An abstract method for pretty-printing the results."""
        pass

    @abstractmethod
    def run(self) -> Self:
        """An abstract method for getting the processed result from openQA.

        This method can be used to refresh the results, for example,
        when the workflow type is manually changed.
        """
        pass

    def __bool__(self) -> bool:
        """Returns `True` if the connector has results, `False` otherwise."""
        return bool(self.results)
