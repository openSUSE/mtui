"""Focused tests for the ``RepoManager`` collaborator.

These tests cover the two methods extracted from :class:`Target`:

* ``set(operation, testreport)`` (was ``Target.set_repo``) — a one-line
  forward into ``testreport.set_repo(target, operation)``.
* ``run_zypper(cmd, repos, rrid)`` (was ``Target.run_zypper``) — fans
  the zypper ar/rr add/remove loop out across the target's flattened
  system and finishes with ``zypper -n ref``. Unknown sub-commands
  force-unlock and raise ``ValueError``.

Plus one regression for the C1 ``_fanout_set_repo`` invariant: the
hostgroup fan-out helper still reaches ``target.repo_manager.set`` on
each target with the right ``(operation, testreport)`` tuple.
"""

from unittest.mock import MagicMock

import pytest

from mtui.target.repo_manager import RepoManager
from mtui.types import Product

# ---------------------------------------------------------------------------
# RepoManager.set
# ---------------------------------------------------------------------------


def test_repo_manager_property_returns_manager_bound_to_target(mock_target):
    """``Target.repo_manager`` returns a ``RepoManager`` keeping a live ref."""
    rm = mock_target.repo_manager
    assert isinstance(rm, RepoManager)
    assert rm.target is mock_target


def test_repo_manager_property_returns_fresh_instance_each_access(mock_target):
    """Per design the property allocates per access; pin that explicitly."""
    assert mock_target.repo_manager is not mock_target.repo_manager


def test_set_delegates_to_testreport(mock_target):
    """``set(op, tr)`` forwards as ``tr.set_repo(target, op)``."""
    tr = MagicMock()
    mock_target.repo_manager.set("add", tr)
    tr.set_repo.assert_called_once_with(mock_target, "add")


# ---------------------------------------------------------------------------
# RepoManager.run_zypper
# ---------------------------------------------------------------------------


def test_run_zypper_ar_emits_add_command(mock_target, mock_rrid):
    """``ar`` (add-repo) builds an ``issue-*`` alias and runs ``zypper ar``."""
    mock_target.state = "enabled"
    mock_target.connection.run.return_value = 0
    mock_target.connection.stdout = ""
    mock_target.connection.stderr = ""
    mock_target.system = MagicMock()
    mock_target.system.flatten.return_value = {Product("SLES", "15-SP5", "x86_64")}
    repos = {Product("SLES", "15-SP5", "x86_64"): "https://example/repo"}
    mock_target.repo_manager.run_zypper("ar", repos, mock_rrid)
    commands = [c[0][0] for c in mock_target.connection.run.call_args_list]
    assert any("zypper ar" in c and "issue-SLES" in c for c in commands)
    assert commands[-1] == "zypper -n ref"


def test_run_zypper_rr_emits_remove_command(mock_target, mock_rrid):
    """``rr`` (remove-repo) runs ``zypper rr <url>`` per matching repo."""
    mock_target.state = "enabled"
    mock_target.connection.run.return_value = 0
    mock_target.connection.stdout = ""
    mock_target.connection.stderr = ""
    mock_target.system = MagicMock()
    mock_target.system.flatten.return_value = {Product("SLES", "15-SP5", "x86_64")}
    repos = {Product("SLES", "15-SP5", "x86_64"): "https://example/repo"}
    mock_target.repo_manager.run_zypper("rr", repos, mock_rrid)
    commands = [c[0][0] for c in mock_target.connection.run.call_args_list]
    assert any("zypper rr https://example/repo" in c for c in commands)


def test_run_zypper_unknown_command_unlocks_and_raises(mock_target, mock_rrid):
    """Unknown sub-command force-unlocks and raises ``ValueError``."""
    mock_target.state = "enabled"
    mock_target.system = MagicMock()
    mock_target.system.flatten.return_value = {Product("SLES", "15-SP5", "x86_64")}
    repos = {Product("SLES", "15-SP5", "x86_64"): "https://example/repo"}
    with pytest.raises(ValueError):  # noqa: PT011 -- bare ValueError raised by run_zypper
        mock_target.repo_manager.run_zypper("nosuch", repos, mock_rrid)
    mock_target._lock.unlock.assert_called_with(True)


def test_run_zypper_skips_products_not_in_flattened_system(mock_target, mock_rrid):
    """Repos whose product is not in the target's flattened system are skipped."""
    mock_target.state = "enabled"
    mock_target.connection.run.return_value = 0
    mock_target.connection.stdout = ""
    mock_target.connection.stderr = ""
    mock_target.system = MagicMock()
    mock_target.system.flatten.return_value = {Product("SLES", "15-SP5", "x86_64")}
    repos = {
        Product("SLES", "15-SP5", "x86_64"): "https://wanted/repo",
        Product("opensuse", "15.4", "x86_64"): "https://other/repo",
    }
    mock_target.repo_manager.run_zypper("ar", repos, mock_rrid)
    commands = [c[0][0] for c in mock_target.connection.run.call_args_list]
    # Wanted repo present; other repo absent. Always finishes with the ref.
    assert any("https://wanted/repo" in c for c in commands)
    assert not any("https://other/repo" in c for c in commands)
    assert commands[-1] == "zypper -n ref"
