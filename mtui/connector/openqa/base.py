from abc import ABC, abstractmethod
from logging import getLogger
from typing import ClassVar, Self

from openqa_client.client import OpenQA_Client as oqa
import openqa_client.exceptions

from .. import SMELT
from ...types import RequestReviewID, Test, URLs

logger = getLogger("mtui.connector.openqa")


class OpenQA(ABC):
    kind: ClassVar[str] = "base"

    def __init__(self, config, host, smelt: SMELT, rrid: RequestReviewID) -> None:
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
        pass

    @abstractmethod
    def run(self) -> Self:
        """Method to get processed result from openQA, can be used for refresh.
        For example when is manually changed type of workflow"""
        pass

    def __bool__(self) -> bool:
        return bool(self.results)
