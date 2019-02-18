from logging import getLogger
from os.path import join
from openqa_client.client import OpenQA_Client
import openqa_client.exceptions
from collections import namedtuple

URLs = namedtuple("URLs", ["distri", "arch", "version", "url"])
logger = getLogger("mtui.connector.openqa")


class Openqa(object):
    def __init__(self):

        self.params = {}
        self.jobs = None

    def get_jobs(self):
        try:
            logger.info("Getting jobs from openQA")
            self.jobs = self.client.openqa_request("GET", "jobs", self.params)["jobs"]
        except (
            openqa_client.exceptions.ConnectionError,
            openqa_client.exceptions.RequestError,
        ) as e:
            logger.error("openqa returned {}".format(e[-1]))
            self.jobs = None

    def postinit(self, config, rrid, smelt):
        self.params["build"] = ":{}:{}".format(
            rrid.maintenance_id, smelt.get_incident_name()
        )
        self.params["distri"] = config.openqa_install_distri
        self.params["scope"] = "relevant"
        self.params["latest"] = 1

        self.test = {}
        self.test["testname"] = config.openqa_install_test
        self.test["logname"] = config.openqa_install_logs
        self.host = config.openqa_instance
        self.client = OpenQA_Client(self.host)

    def pprint_results(self):
        ret = []

        if not self.jobs:
            return ret

        ret.append("Results from incidents openQA jobs:\n")

        for job in self.jobs:
            ret.append(
                "  Job in flavor: {} - arch: {} - test: {} - result: {} \n".format(
                    job["settings"]["FLAVOR"],
                    job["settings"]["ARCH"],
                    job["test"],
                    job["result"],
                )
            )
            failed_modules = [
                (module["name"], module["category"])
                for module in job["modules"]
                if module["result"] == "failed"
            ]
            if failed_modules:
                ret.append("    Failed modules:\n")
                for mod in failed_modules:
                    ret.append("      Module: {} in category {} failed\n".format(*mod))
            ret.append("\n")
        ret.append("End of openQA Incidents results\n")

        return ret

    def get_logs_url(self):
        if not self.jobs:
            return None
        return [
            URLs(
                job["settings"]["HDD_1"].split("-")[0],
                job["settings"]["ARCH"],
                job["settings"]["VERSION"],
                join(
                    self.host,
                    "tests",
                    str(job["id"]),
                    "file",
                    self.test["logname"],
                ),
            )
            for job in self.jobs
            if job["test"] == self.test["testname"]
        ]

    def __bool__(self):
        return bool(self.jobs)
