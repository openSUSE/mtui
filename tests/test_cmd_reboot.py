"""Tests for the `reboot` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.reboot import Reboot
from mtui.hosts.target.hostgroup import HostsGroup
from mtui.support.messages import NoRefhostsDefinedError


def _target(hostname="h1", state="enabled"):
    t = MagicMock()
    t.hostname = hostname
    t.state = state
    return t


def _prompt(targets, lock_comment=""):
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.lock_comment = lock_comment
    p.display = MagicMock()
    p.targets = targets
    return p


def test_reboot_reboots_all_connected(mock_config):
    """No -t: reboot all connected hosts, no PI relock when not testing a PI."""
    hg = HostsGroup([_target("h1"), _target("h2")])
    prompt = _prompt(hg)
    args = Namespace(hosts=None)

    with patch.object(HostsGroup, "reboot") as reboot:
        Reboot(args, mock_config, MagicMock(), prompt)()

    reboot.assert_called_once_with(relock_comment="")


def test_reboot_passes_pi_lock_comment(mock_config):
    """An active PI testing lock is re-applied after reboot."""
    hg = HostsGroup([_target("h1")])
    prompt = _prompt(hg, lock_comment="testing of SUSE:PI:34556:1")
    args = Namespace(hosts=None)

    with patch.object(HostsGroup, "reboot") as reboot:
        Reboot(args, mock_config, MagicMock(), prompt)()

    reboot.assert_called_once_with(relock_comment="testing of SUSE:PI:34556:1")


def test_reboot_selects_targets_with_t(mock_config):
    """-t selects only the named hosts."""
    hg = HostsGroup([_target("h1"), _target("h2")])
    prompt = _prompt(hg)
    args = Namespace(hosts=["h2"])

    with patch.object(HostsGroup, "reboot") as reboot:
        Reboot(args, mock_config, MagicMock(), prompt)()

    # reboot() is called on the selected subgroup (which contains only h2).
    reboot.assert_called_once_with(relock_comment="")


def test_reboot_no_hosts_raises(mock_config):
    """No connected hosts -> NoRefhostsDefinedError."""
    prompt = _prompt(HostsGroup([]))
    args = Namespace(hosts=None)

    with pytest.raises(NoRefhostsDefinedError):
        Reboot(args, mock_config, MagicMock(), prompt)()
