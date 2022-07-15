import json
import logging
from pathlib import Path

import pytest
import responses

from mtui.messages import RepositoryError
from mtui.connector.smelt import SMELT
from mtui.types.obs import RequestReviewID
from mtui.config import Config


_rootdir = Path(__file__).resolve().parent

incident_12538 = json.loads(_rootdir.joinpath("./fixtures/inc_12358s.json").read_text())
incident_12538nl = json.loads(
    _rootdir.joinpath("./fixtures/inc_12358nl.json").read_text()
)
incident_12538nc = json.loads(
    _rootdir.joinpath("./fixtures/inc_12358nc.json").read_text()
)

q_url = Config(_rootdir / "fixtures/mtuirc").smelt_api


@responses.activate
def helper_smelt_instance(kind):
    if kind == "full":
        responses.add(
            responses.GET,
            q_url,
            json=incident_12538,
            match_querystring=False,
            status=200,
        )
    elif kind == "checkers_only":
        responses.add(
            responses.GET,
            q_url,
            json=incident_12538nl,
            match_querystring=False,
            status=200,
        )
    elif kind == "comments_only":
        responses.add(
            responses.GET,
            q_url,
            json=incident_12538nc,
            match_querystring=False,
            status=200,
        )
    instance = SMELT(RequestReviewID("SUSE:Maintenance:12358:199773"))

    return instance


def test_nodata():
    x = SMELT(RequestReviewID("SUSE:Maintenance:1:1"))
    assert not x
    assert not x.openqa_links()
    assert not x.get_incident_name()
    assert not x.get_version()


def test_openqa_link():
    links = helper_smelt_instance("full").openqa_links()
    assert links == [
        "https://openqa.suse.de/tests/overview?version=12-SP2&groupid=53&flavor=Server-DVD-Incidents-Minimal&distri=sle&build=%3A12358%3Akernel-ec2",
        "https://openqa.suse.de/tests/overview?version=12-SP2&groupid=54&flavor=Server-DVD-Updates&distri=sle&build=20190904-1",
    ]


def test_openqa_link_empty():
    links = helper_smelt_instance("checkers_only").openqa_links()
    assert not links


def test_openqa_link_verbose():
    links = helper_smelt_instance("comments_only").openqa_links_verbose()
    assert links == [
        "Maintenance: SLE 12 SP2 Incidents@Server-DVD-Incidents-Minimal:",
        "  link: https://openqa.suse.de/tests/overview?version=12-SP2&groupid=53&flavor=Server-DVD-Incidents-Minimal&distri=sle&build=%3A12358%3Akernel-ec2",
        "    results: 8 tests passed",
        "Maintenance: SLE 12 SP2 Updates@Server-DVD-Updates:",
        "  link: https://openqa.suse.de/tests/overview?version=12-SP2&groupid=54&flavor=Server-DVD-Updates&distri=sle&build=20190904-1",
        "    results: 33 tests passed, 5 tests failed",
    ]


def test_openqa_link_verbose_empty():
    links = helper_smelt_instance("checkers_only").openqa_links_verbose()
    assert not links


def test_incident_name():
    name = helper_smelt_instance("full").get_incident_name()
    assert name == "kernel-ec2"


def test_get_version():
    version = helper_smelt_instance("full").get_version()
    assert version == "12-SP2"
