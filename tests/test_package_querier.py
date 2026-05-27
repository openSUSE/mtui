"""Focused tests for ``PackageQuerier`` (the rpm/dpkg branch).

These tests pin the contracts exposed by
``mtui.target.package_querier.PackageQuerier``:

* the rpm vs dpkg branch keyed on ``system.get_base().name``,
* the ``"package X is not installed"`` line → ``None`` mapping,
* per-name "keep the highest version" dedup,
* empty-output edge.

The pre-existing characterization tests in ``tests/test_target.py``
(``test_query_package_versions_*``) cover the same surface through the
``Target.query_package_versions`` delegate; these add explicit coverage
at the collaborator boundary.
"""

from unittest.mock import MagicMock

from mtui.target.package_querier import PackageQuerier
from mtui.types.rpmver import RPMVersion


def _querier_with_base(base_name: str = "SLES", stdout: str = ""):
    """Build a PackageQuerier whose target advertises ``base_name`` and ``stdout``."""
    target = MagicMock()
    target.system.get_base.return_value = MagicMock(name="Product")
    target.system.get_base.return_value.name = base_name
    target.lastout.return_value = stdout
    return PackageQuerier(target), target  # ty: ignore[invalid-argument-type]


def test_non_ubuntu_base_uses_rpm_query():
    """Non-ubuntu bases route through ``rpm -q --queryformat``."""
    pq, t = _querier_with_base("SLES", stdout="bash 5.1-1\n")
    pq.versions(["bash"])
    cmd = t.run.call_args[0][0]
    assert cmd.startswith("rpm -q --queryformat")
    assert "%{Name}" in cmd
    assert "%{Version}" in cmd
    assert cmd.endswith(" bash")


def test_ubuntu_base_uses_dpkg_query():
    """Ubuntu base routes through ``dpkg-query``."""
    pq, t = _querier_with_base("ubuntu", stdout="bash 5.1-1\n")
    pq.versions(["bash"])
    cmd = t.run.call_args[0][0]
    assert cmd.startswith("dpkg-query")
    assert "${package}" in cmd
    assert "${version}" in cmd
    assert cmd.endswith(" bash")


def test_joins_multiple_packages_into_command():
    """The full package list is joined into one command, not one per package."""
    pq, t = _querier_with_base("SLES", stdout="bash 5.1-1\nopenssl 3.0-1\n")
    pq.versions(["bash", "openssl"])
    cmd = t.run.call_args[0][0]
    assert cmd.endswith(" bash openssl")
    # Exactly one rpm invocation.
    assert t.run.call_count == 1


def test_returns_version_per_installed_package():
    """Each line yields a ``name -> RPMVersion`` entry."""
    pq, _ = _querier_with_base("SLES", stdout="bash 5.1-1\nopenssl 3.0-1\n")
    out = pq.versions(["bash", "openssl"])
    assert out == {"bash": RPMVersion("5.1-1"), "openssl": RPMVersion("3.0-1")}


def test_not_installed_line_yields_none():
    """``"package X is not installed"`` maps the name to ``None``."""
    pq, _ = _querier_with_base("SLES", stdout="package missing-pkg is not installed\n")
    out = pq.versions(["missing-pkg"])
    assert out == {"missing-pkg": None}


def test_duplicate_lines_collapse_to_highest_version():
    """Multiple rpm-q lines for the same name keep the highest version."""
    pq, _ = _querier_with_base("SLES", stdout="bash 5.0-1\nbash 5.2-1\nbash 5.1-1\n")
    out = pq.versions(["bash"])
    assert out == {"bash": RPMVersion("5.2-1")}


def test_empty_stdout_returns_empty_dict():
    """No output → empty mapping. Defensive against transactional/disabled hosts."""
    pq, _ = _querier_with_base("SLES", stdout="")
    out = pq.versions(["bash"])
    assert out == {}
