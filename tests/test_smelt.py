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
    assert not x.pretty_output()
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


def test_pretty_output_empty():
    output = helper_smelt_instance("comments_only").pretty_output()
    assert output == []


def test_pretty_output():
    output = helper_smelt_instance( "checkers_only").pretty_output()
    assert output == [
        "Incident checker:\n",
        "    product:  /  arch: all\n",
        "        SUSE:Maintenance:12358: Missing fixes: bsc#1000199 bsc#1000204 "
        "bsc#1031392 bsc#1058186 bsc#1070936 bsc#1074556 bsc#1083483 bsc#1087084 "
        "bsc#1089608 bsc#1090643 bsc#1093590 bsc#1102851 bsc#1103203 bsc#1107866 "
        "bsc#1108302 bsc#1109387 bsc#1111609 bsc#1117665 bsc#1120260 bsc#1121872 "
        "bsc#1123959 bsc#1124763 bsc#1129735 bsc#1129898 bsc#1131326 bsc#1131645 "
        "bsc#1131980 bsc#1133106 bsc#1133374 bsc#1134561 bsc#1135603 bsc#1135966 "
        "bsc#1135967 bsc#1136446 bsc#1137586 bsc#1137865 bsc#1137944 bsc#1139073 "
        "bsc#1139751 bsc#1142254 bsc#1142428 bsc#1143187 bsc#1144286 bsc#1144903 "
        "bsc#1145477 bsc#1146285 bsc#1146312 bsc#1146361 bsc#1146378 bsc#1146391 "
        "bsc#1146413 bsc#1146425 bsc#1146512 bsc#1146514 bsc#1146516 bsc#1146519 "
        "bsc#1146524 bsc#1146526 bsc#1146529 bsc#1146543 bsc#1146544 bsc#1146547 "
        "bsc#1146550 bsc#1146584 bsc#1146589 bsc#1148394 bsc#1148938 bsc#317769 "
        "bsc#907150 bsc#920615 bsc#930408\n",
        "        SUSE:Maintenance:12358: Issues added to changelog but missing in "
        "patchinfo: CVE-2019-11478 CVE-2019-13648 CVE-2019-3846 bsc#1123959 "
        "bsc#1136424 bsc#1137586 bsc#1139751 bsc#1142265 bsc#1144286\n",
        "\n",
        "Install checker:\n",
        "    product: SLE-SERVER / 12-SP2-LTSS arch: ppc64le\n",
        "        can't install kgraft-patch-4_4_121-92_120-default-1-3.3.1.ppc64le:\n",
        "          nothing provides kgraft needed by "
        "kgraft-patch-4_4_121-92_120-default-1-3.3.1.ppc64le\n",
        "\n",
        "Install checker:\n",
        "    product: SLE-HA / 12-SP2 arch: ppc64le\n",
        "        can't install kgraft-patch-4_4_121-92_120-default-1-3.3.1.ppc64le:\n",
        "          nothing provides kgraft needed by "
        "kgraft-patch-4_4_121-92_120-default-1-3.3.1.ppc64le\n",
        "\n",
        "Install checker:\n",
        "    product: SLE-SAP / 12-SP2 arch: ppc64le\n",
        "        can't install kgraft-patch-4_4_121-92_120-default-1-3.3.1.ppc64le:\n",
        "          nothing provides kgraft needed by "
        "kgraft-patch-4_4_121-92_120-default-1-3.3.1.ppc64le\n",
        "\n",
        "Install checker:\n",
        "    product: Storage / 4 arch: x86_64\n",
        "        can't install kgraft-patch-4_4_121-92_120-default-1-3.3.1.x86_64:\n",
        "          nothing provides kgraft needed by "
        "kgraft-patch-4_4_121-92_120-default-1-3.3.1.x86_64\n",
        "\n",
        "Install checker:\n",
        "    product: SLE-HA / 12-SP2 arch: x86_64\n",
        "        can't install kgraft-patch-4_4_121-92_120-default-1-3.3.1.x86_64:\n",
        "          nothing provides kgraft needed by "
        "kgraft-patch-4_4_121-92_120-default-1-3.3.1.x86_64\n",
        "\n",
        "Install checker:\n",
        "    product: SLE-SERVER / 12-SP2-ESPOS arch: x86_64\n",
        "        can't install kgraft-patch-4_4_121-92_120-default-1-3.3.1.x86_64:\n",
        "          nothing provides kgraft needed by "
        "kgraft-patch-4_4_121-92_120-default-1-3.3.1.x86_64\n",
        "\n",
        "Install checker:\n",
        "    product: SLE-SERVER / 12-SP2-LTSS arch: x86_64\n",
        "        can't install kgraft-patch-4_4_121-92_120-default-1-3.3.1.x86_64:\n",
        "          nothing provides kgraft needed by "
        "kgraft-patch-4_4_121-92_120-default-1-3.3.1.x86_64\n",
        "\n",
        "Install checker:\n",
        "    product: SLE-POS / 12-SP2-CLIENT arch: x86_64\n",
        "        can't install kgraft-patch-4_4_121-92_120-default-1-3.3.1.x86_64:\n",
        "          nothing provides kgraft needed by "
        "kgraft-patch-4_4_121-92_120-default-1-3.3.1.x86_64\n",
        "\n",
        "Install checker:\n",
        "    product: SLE-SAP / 12-SP2 arch: x86_64\n",
        "        can't install kgraft-patch-4_4_121-92_120-default-1-3.3.1.x86_64:\n",
        "          nothing provides kgraft needed by "
        "kgraft-patch-4_4_121-92_120-default-1-3.3.1.x86_64\n",
        "\n",
        "Install checker:\n",
        "    product: OpenStack-Cloud / 7 arch: x86_64\n",
        "        can't install kgraft-patch-4_4_121-92_120-default-1-3.3.1.x86_64:\n",
        "          nothing provides kgraft needed by "
        "kgraft-patch-4_4_121-92_120-default-1-3.3.1.x86_64\n",
        "\n",
        "Patch checker:\n",
        "    product: SLE-SAP / 12-SP2 arch: ppc64le\n",
        "        can't install patch:SUSE-SLE-SAP-12-SP2-2019-12358-1.noarch:\n",
        "          package ocfs2-kmp-default-4.4.21-69.1.ppc64le requires "
        "kernel-default = 4.4.21-69.1, but none of the providers can be installed\n",
        "          package patch:SUSE-SLE-SAP-12-SP2-2019-12358-1.noarch conflicts "
        "with kernel-default.ppc64le < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.ppc64le\n",
        "          package patch:SUSE-SLE-SAP-12-SP2-2019-12358-1.noarch conflicts "
        "with kernel-default.ppc64le < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.ppc64le\n",
        "          update rule for ocfs2-kmp-default-4.4.21-69.1.ppc64le\n",
        "        can't install patch:SUSE-SLE-SAP-12-SP2-2019-12358-1.noarch:\n",
        "          package gfs2-kmp-default-4.4.21-69.1.ppc64le requires "
        "kernel-default = 4.4.21-69.1, but none of the providers can be installed\n",
        "          package patch:SUSE-SLE-SAP-12-SP2-2019-12358-1.noarch conflicts "
        "with kernel-default.ppc64le < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.ppc64le\n",
        "          package patch:SUSE-SLE-SAP-12-SP2-2019-12358-1.noarch conflicts "
        "with kernel-default.ppc64le < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.ppc64le\n",
        "          update rule for gfs2-kmp-default-4.4.21-69.1.ppc64le\n",
        "        can't install patch:SUSE-SLE-SAP-12-SP2-2019-12358-1.noarch:\n",
        "          package dlm-kmp-default-4.4.21-69.1.ppc64le requires "
        "kernel-default = 4.4.21-69.1, but none of the providers can be installed\n",
        "          package patch:SUSE-SLE-SAP-12-SP2-2019-12358-1.noarch conflicts "
        "with kernel-default.ppc64le < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.ppc64le\n",
        "          package patch:SUSE-SLE-SAP-12-SP2-2019-12358-1.noarch conflicts "
        "with kernel-default.ppc64le < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.ppc64le\n",
        "          update rule for dlm-kmp-default-4.4.21-69.1.ppc64le\n",
        "        can't install patch:SUSE-SLE-SAP-12-SP2-2019-12358-1.noarch:\n",
        "          package cluster-network-kmp-default-4.4.21-69.1.ppc64le requires "
        "kernel-default = 4.4.21-69.1, but none of the providers can be installed\n",
        "          package patch:SUSE-SLE-SAP-12-SP2-2019-12358-1.noarch conflicts "
        "with kernel-default.ppc64le < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.ppc64le\n",
        "          package patch:SUSE-SLE-SAP-12-SP2-2019-12358-1.noarch conflicts "
        "with kernel-default.ppc64le < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.ppc64le\n",
        "          update rule for cluster-network-kmp-default-4.4.21-69.1.ppc64le\n",
        "\n",
        "Patch checker:\n",
        "    product: SLE-SAP / 12-SP2 arch: x86_64\n",
        "        can't install "
        "patch:SUSE-SLE-SERVER-12-SP2-ESPOS-2019-12358-1.noarch:\n",
        "          package ocfs2-kmp-default-4.4.21-69.1.x86_64 requires "
        "kernel-default = 4.4.21-69.1, but none of the providers can be installed\n",
        "          package patch:SUSE-SLE-SERVER-12-SP2-ESPOS-2019-12358-1.noarch "
        "conflicts with kernel-default.x86_64 < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.x86_64\n",
        "          package patch:SUSE-SLE-SERVER-12-SP2-ESPOS-2019-12358-1.noarch "
        "conflicts with kernel-default.x86_64 < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.x86_64\n",
        "          update rule for ocfs2-kmp-default-4.4.21-69.1.x86_64\n",
        "        can't install "
        "patch:SUSE-SLE-SERVER-12-SP2-ESPOS-2019-12358-1.noarch:\n",
        "          package gfs2-kmp-default-4.4.21-69.1.x86_64 requires "
        "kernel-default = 4.4.21-69.1, but none of the providers can be installed\n",
        "          package patch:SUSE-SLE-SERVER-12-SP2-ESPOS-2019-12358-1.noarch "
        "conflicts with kernel-default.x86_64 < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.x86_64\n",
        "          package patch:SUSE-SLE-SERVER-12-SP2-ESPOS-2019-12358-1.noarch "
        "conflicts with kernel-default.x86_64 < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.x86_64\n",
        "          update rule for gfs2-kmp-default-4.4.21-69.1.x86_64\n",
        "        can't install "
        "patch:SUSE-SLE-SERVER-12-SP2-ESPOS-2019-12358-1.noarch:\n",
        "          package dlm-kmp-default-4.4.21-69.1.x86_64 requires kernel-default "
        "= 4.4.21-69.1, but none of the providers can be installed\n",
        "          package patch:SUSE-SLE-SERVER-12-SP2-ESPOS-2019-12358-1.noarch "
        "conflicts with kernel-default.x86_64 < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.x86_64\n",
        "          package patch:SUSE-SLE-SERVER-12-SP2-ESPOS-2019-12358-1.noarch "
        "conflicts with kernel-default.x86_64 < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.x86_64\n",
        "          update rule for dlm-kmp-default-4.4.21-69.1.x86_64\n",
        "        can't install "
        "patch:SUSE-SLE-SERVER-12-SP2-ESPOS-2019-12358-1.noarch:\n",
        "          package cluster-network-kmp-default-4.4.21-69.1.x86_64 requires "
        "kernel-default = 4.4.21-69.1, but none of the providers can be installed\n",
        "          package patch:SUSE-SLE-SERVER-12-SP2-ESPOS-2019-12358-1.noarch "
        "conflicts with kernel-default.x86_64 < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.x86_64\n",
        "          package patch:SUSE-SLE-SERVER-12-SP2-ESPOS-2019-12358-1.noarch "
        "conflicts with kernel-default.x86_64 < 4.4.121-92.120.1 provided by "
        "kernel-default-4.4.21-69.1.x86_64\n",
        "          update rule for cluster-network-kmp-default-4.4.21-69.1.x86_64\n",
        "\n",
    ]


def test_get_version():
    version = helper_smelt_instance("full").get_version()
    assert version == "12-SP2"
