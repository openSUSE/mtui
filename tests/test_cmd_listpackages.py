"""Tests for the `list_packages` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.listpackages import ListPackages
from mtui.support.messages import MissingPackagesError, TestReportNotLoadedError


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.metadata.packages = {"15-SP5": {"bash": "5.1-1.1"}}
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_list_packages_wanted_prints_versions(mock_config):
    prompt = _prompt()
    sys_mock = MagicMock()
    args = Namespace(wanted=True, package=[], hosts=None)

    ListPackages(args, mock_config, sys_mock, prompt)()

    written = "".join(c.args[0] for c in sys_mock.stdout.write.call_args_list)
    assert "Packages for version 15-SP5" in written
    assert "bash" in written


def test_list_packages_empty_raises_missing_packages(mock_config):
    prompt = _prompt()
    prompt.metadata.get_package_list.return_value = []
    prompt.targets.select.return_value = MagicMock()
    args = Namespace(wanted=False, package=[], hosts=None)

    with pytest.raises(MissingPackagesError):
        ListPackages(args, mock_config, MagicMock(), prompt)()


def test_list_packages_wanted_without_metadata_raises(mock_config):
    prompt = _prompt()
    prompt.metadata.__bool__ = lambda self: False
    args = Namespace(wanted=True, package=[], hosts=None)
    with pytest.raises(TestReportNotLoadedError):
        ListPackages(args, mock_config, MagicMock(), prompt)()


def test_list_packages_is_fanout():
    assert ListPackages.scope == "fanout"


def test_list_packages_accepts_template_flags():
    sys_mock = MagicMock()
    ns = ListPackages.parse_args("-T SUSE:Maintenance:1:1", sys_mock)
    assert ns.template == "SUSE:Maintenance:1:1"
    assert ListPackages.parse_args("--all-templates", sys_mock).all_templates is True


def test_list_packages_blank_state_for_package_not_in_update(mock_config):
    """A queried package that is not part of the update gets a blank state.

    The KeyError branch set ``state = None``, which the output formatter
    rendered as the literal word "None" -- inconsistent with the
    no-metadata branch, which uses "" for the same no-state case.
    """
    prompt = _prompt()
    prompt.metadata.get_package_list.return_value = ["bash"]
    target = MagicMock()
    target.hostname = "host1"
    target.system = "sys"
    target.packages = {}  # nothing from the update on this host -> KeyError
    hosts = MagicMock()
    hosts.query_versions.return_value = [(target, {"somepkg": "1.0"})]
    prompt.targets.select.return_value = hosts
    sys_mock = MagicMock()
    args = Namespace(wanted=False, package=["somepkg"], hosts=None)

    ListPackages(args, mock_config, sys_mock, prompt)()

    written = "".join(c.args[0] for c in sys_mock.stdout.write.call_args_list)
    assert "somepkg" in written
    assert "None" not in written


def test_list_packages_not_installed_state_for_absent_package(mock_config):
    """A queried package neither in the update nor installed says so.

    The KeyError branch must mirror the no-metadata branch exactly: the
    querier returns None for a package rpm reports as not installed, and
    the row must read "not installed" -- with no literal "None" anywhere
    (the version column used to render one too).
    """
    prompt = _prompt()
    prompt.metadata.get_package_list.return_value = ["bash"]
    target = MagicMock()
    target.hostname = "host1"
    target.system = "sys"
    target.packages = {}
    hosts = MagicMock()
    hosts.query_versions.return_value = [(target, {"ghostpkg": None})]
    prompt.targets.select.return_value = hosts
    sys_mock = MagicMock()
    args = Namespace(wanted=False, package=["ghostpkg"], hosts=None)

    ListPackages(args, mock_config, sys_mock, prompt)()

    written = "".join(c.args[0] for c in sys_mock.stdout.write.call_args_list)
    assert "ghostpkg" in written
    assert "not installed" in written
    assert "None" not in written
