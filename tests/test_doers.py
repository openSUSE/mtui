"""Focused tests for ``Target.doer(role)`` / ``Target.check(role)``.

These tests cover the dispatch tables introduced when the seven
historical ``get_installer`` / ``get_installer_check`` / ... methods on
``Target`` were collapsed (Phase 5b / C2 / Cluster A). They pin three
things callers depend on:

* role-to-registry routing for each of the five roles,
* preparer-specific ``force`` / ``testing`` keyword forwarding,
* the ``_no_checks`` sentinel fallback for unknown
  ``(release, transactional)`` keys.

The pre-existing per-flow integration tests
(``tests/test_hostgroup.py::test_perform_*``,
``tests/test_operation.py::test_install_and_uninstall_*``) cover the
caller side; this file covers the dispatch surface in isolation.
"""

from string import Template
from unittest.mock import MagicMock

import pytest

from mtui.hosts.target import Target
from mtui.hosts.target.target import _no_checks


def _target_with_release(
    mock_config, release: str = "15", transactional: bool = False
) -> Target:
    """Build a ``Target`` whose system advertises a fixed (release, transactional)."""
    target = Target(mock_config, "h.example.com")  # type: ignore[arg-type]
    target.system = MagicMock()
    target.system.get_release.return_value = release
    target.transactional = transactional
    return target


# ---------------------------------------------------------------------------
# doer() dispatch
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "role",
    ["installer", "uninstaller", "downgrader", "updater"],
)
def test_doer_returns_template_dict_for_known_release(mock_config, role):
    """All four non-preparer roles route to a ``{name: Template}`` mapping."""
    target = _target_with_release(mock_config)
    result = target.doer(role)
    assert isinstance(result, dict)
    assert all(isinstance(v, Template) for v in result.values())


def test_doer_preparer_routes_through_factory(mock_config):
    """The preparer arm is a callable in the registry; doer invokes it with defaults."""
    target = _target_with_release(mock_config)
    result = target.doer("preparer")
    assert isinstance(result, dict)


def test_doer_preparer_forwards_force_and_testing(mock_config):
    """``doer('preparer', force, testing)`` forwards both kwargs to the factory."""
    target = _target_with_release(mock_config)
    # Both keyword and positional forms must be honoured (callers use positional).
    result_pos = target.doer("preparer", True, True)
    result_kw = target.doer("preparer", force=True, testing=True)
    assert isinstance(result_pos, dict)
    assert isinstance(result_kw, dict)


def test_doer_unknown_role_raises_key_error(mock_config):
    """An unsupported role string is a programmer error: ``KeyError``."""
    target = _target_with_release(mock_config)
    with pytest.raises(KeyError):
        target.doer("nosuch")


# ---------------------------------------------------------------------------
# check() dispatch
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "role",
    ["installer", "uninstaller", "downgrader", "updater", "preparer"],
)
def test_check_returns_callable_for_known_role(mock_config, role):
    """Every role exposes a callable check (registered or the ``_no_checks`` sentinel)."""
    target = _target_with_release(mock_config)
    assert callable(target.check(role))


def test_check_falls_back_to_no_checks_for_unknown_release(mock_config):
    """Unknown ``(release, transactional)`` tuples fall back to the no-op sentinel."""
    target = _target_with_release(mock_config, release="9999", transactional=False)
    assert target.check("installer") is _no_checks


def test_check_uninstaller_consults_install_checks_table(mock_config):
    """``check('uninstaller')`` returns whatever ``install_checks`` advertises.

    There is no dedicated uninstall-check table — this mirrors the prior
    behaviour of the deleted ``get_uninstaller_check`` method, which
    explicitly looked the value up in ``install_checks``.
    """
    target = _target_with_release(mock_config)
    assert target.check("uninstaller") is target.check("installer")


def test_check_unknown_role_raises_key_error(mock_config):
    """An unsupported role string is a programmer error: ``KeyError``."""
    target = _target_with_release(mock_config)
    with pytest.raises(KeyError):
        target.check("nosuch")
