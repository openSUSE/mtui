"""Tests for ``mtui.test_reports.testreport.TestReport`` (via ``OBSTestReport``)."""

from __future__ import annotations

import logging
from io import StringIO
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from mtui.support.exceptions import UpdateError
from mtui.test_reports.obs_report import OBSTestReport
from mtui.types import RequestReviewID


def _config(tmp_path: Path) -> MagicMock:
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.reports_url = "https://reports.example/"
    cfg.fancy_reports_url = "https://fancy.example/"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    return cfg


def _make(tmp_path: Path) -> OBSTestReport:
    r = OBSTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    return r


# ---------------------------------------------------------------------------
# Package list aggregation + dedup
# ---------------------------------------------------------------------------


def test_get_package_list_dedups_across_versions(tmp_path: Path) -> None:
    r = _make(tmp_path)
    r.packages = {"v1": ["bash", "openssl"], "v2": ["bash", "libfoo"]}
    out = r.get_package_list()
    assert sorted(out) == ["bash", "libfoo", "openssl"]


# ---------------------------------------------------------------------------
# PI auto-lock: locking freshly connected targets
# ---------------------------------------------------------------------------


def test_autolock_new_target_locks_when_comment_set(tmp_path: Path) -> None:
    r = _make(tmp_path)
    r.lock_comment = "testing of SUSE:PI:34556:1"
    target = MagicMock()
    r._autolock_new_target(target)  # ty: ignore[invalid-argument-type]
    target.lock.assert_called_once_with("testing of SUSE:PI:34556:1")


def test_autolock_new_target_noop_without_comment(tmp_path: Path) -> None:
    r = _make(tmp_path)
    assert r.lock_comment == ""
    target = MagicMock()
    r._autolock_new_target(target)  # ty: ignore[invalid-argument-type]
    target.lock.assert_not_called()


def test_autolock_new_target_suppresses_foreign_lock(tmp_path: Path) -> None:
    from mtui.hosts.target import TargetLockedError

    r = _make(tmp_path)
    r.lock_comment = "testing of SUSE:PI:34556:1"
    target = MagicMock()
    target.lock.side_effect = TargetLockedError("locked by someone else")
    # A host already locked by another user must not abort the connect flow.
    r._autolock_new_target(target)  # ty: ignore[invalid-argument-type]
    target.lock.assert_called_once()


def test_get_package_list_empty(tmp_path: Path) -> None:
    r = _make(tmp_path)
    r.packages = {}
    assert r.get_package_list() == []


# ---------------------------------------------------------------------------
# _verify_target_products: drift check against refhosts.yml
# ---------------------------------------------------------------------------


def _target_with_system(hostname, base, addons=(), dangling=False) -> MagicMock:
    from mtui.types import Product as DetectedProduct
    from mtui.types.systems import System

    target = MagicMock()
    target.hostname = hostname
    target.system = System(
        DetectedProduct(*base),
        {DetectedProduct(*a) for a in addons},
        dangling_base=dangling,
    )
    return target


def _refhost(name, arch, product, addons=()):
    from mtui.hosts.refhost.models import Addon, Host, Product, Version

    pname, major, minor = product
    return Host(
        name=name,
        arch=arch,
        product=Product(name=pname, version=Version(major=major, minor=minor)),
        addons=tuple(
            Addon(name=n, version=Version(major=amaj, minor=amin))
            for n, amaj, amin in addons
        ),
    )


def _stub_store(r: OBSTestReport, host) -> MagicMock:
    """Wire a pre-built refhosts store whose host_by_name returns ``host``."""
    store = MagicMock()
    store.host_by_name.return_value = host
    r._refhosts_store = store
    r._refhosts_store_built = True
    return store


def test_verify_target_products_warns_on_drift(tmp_path: Path, caplog) -> None:
    r = _make(tmp_path)
    target = _target_with_system("h1", ("SLES", "16.0", "x86_64"))
    # Metadata says aarch64 -> arch drift.
    _stub_store(r, _refhost("h1", "aarch64", ("SLES", 16, 0)))
    with caplog.at_level(logging.WARNING, logger="mtui.template.testreport"):
        r._verify_target_products(target)
    assert "h1" in r.product_warnings
    assert any("refhosts.yml" in rec.message for rec in caplog.records)


def test_verify_target_products_clears_stale_warning_on_match(tmp_path: Path) -> None:
    r = _make(tmp_path)
    target = _target_with_system("h1", ("SLES", "16.0", "x86_64"))
    _stub_store(r, _refhost("h1", "x86_64", ("SLES", 16, 0)))
    r.product_warnings["h1"] = ["stale"]
    r._verify_target_products(target)
    assert "h1" not in r.product_warnings


def test_verify_target_products_skips_host_not_in_metadata(tmp_path: Path) -> None:
    r = _make(tmp_path)
    target = _target_with_system("h1", ("SLES", "16.0", "x86_64"))
    _stub_store(r, None)
    r._verify_target_products(target)
    assert r.product_warnings == {}


def test_verify_target_products_is_best_effort(tmp_path: Path) -> None:
    """A failure in the check never propagates out of the connect path."""
    r = _make(tmp_path)
    target = _target_with_system("h1", ("SLES", "16.0", "x86_64"))
    store = _stub_store(r, None)
    store.host_by_name.side_effect = RuntimeError("boom")
    # Must not raise.
    r._verify_target_products(target)
    assert r.product_warnings == {}


# ---------------------------------------------------------------------------
# _warn_missing_fields
# ---------------------------------------------------------------------------


def test_warn_missing_fields_logs_when_attrs_blank(tmp_path: Path, caplog) -> None:
    r = _make(tmp_path)
    # category, packager, reviewer, repository, packages, bugs default to "" / {}
    with caplog.at_level(logging.WARNING, logger="mtui.template.testreport"):
        r._warn_missing_fields()
    assert any("missing fields" in rec.message for rec in caplog.records)


def test_warn_missing_fields_silent_when_populated(tmp_path: Path, caplog) -> None:
    r = _make(tmp_path)
    # Fill every _attr so nothing reports missing.
    for a in r._attrs:
        current = getattr(r, a)
        if isinstance(current, dict):
            setattr(r, a, {"k": "v"})
        elif isinstance(current, list):
            setattr(r, a, ["x"])
        else:
            setattr(r, a, "filled")
    with caplog.at_level(logging.WARNING, logger="mtui.template.testreport"):
        r._warn_missing_fields()
    assert not any("missing fields" in rec.message for rec in caplog.records)


# ---------------------------------------------------------------------------
# _aligned_write
# ---------------------------------------------------------------------------


def test_aligned_write_skips_empty_values(tmp_path: Path) -> None:
    r = _make(tmp_path)
    buf = StringIO()
    r._aligned_write(buf, [("a", "x"), ("b", ""), ("c", "y")])
    text = buf.getvalue()
    assert "a" in text
    assert "x" in text
    assert "c" in text
    assert "y" in text
    # No empty line for "b" with no value.
    assert "b              " not in text


# ---------------------------------------------------------------------------
# URL helpers
# ---------------------------------------------------------------------------


def test_testreport_url_uses_reports_url(tmp_path: Path) -> None:
    r = _make(tmp_path)
    url = r._testreport_url()
    assert url.startswith("https://reports.example/")
    assert "log" in url


def test_fancy_report_url_uses_fancy_reports_url(tmp_path: Path) -> None:
    r = _make(tmp_path)
    url = r.fancy_report_url()
    assert url.startswith("https://fancy.example/")


# ---------------------------------------------------------------------------
# target_wd
# ---------------------------------------------------------------------------


def test_target_wd_returns_id_joined_path(tmp_path: Path) -> None:
    r = _make(tmp_path)
    out = r.target_wd("foo", "bar")
    assert str(out).endswith(f"{r.id}/foo/bar")


# ---------------------------------------------------------------------------
# _show_yourself_data structure (happy)
# ---------------------------------------------------------------------------


def test_show_yourself_data_returns_pairs(tmp_path: Path) -> None:
    r = _make(tmp_path)
    r.testplatforms = ["tp1"]
    r.products = ["sles-15"]
    rows = r._show_yourself_data()
    assert all(len(p) == 2 for p in rows)
    keys = [k for k, _ in rows]
    assert "Category" in keys
    assert "Testplatform" in keys
    assert "Products" in keys


# ---------------------------------------------------------------------------
# list_bugs delegates
# ---------------------------------------------------------------------------


def test_list_bugs_delegates_to_sink(tmp_path: Path) -> None:
    r = _make(tmp_path)
    r.bugs = {"123": "fix"}
    r.jira = {"J-1": "story"}
    sink = MagicMock(return_value="ok")
    out = r.list_bugs(sink, "extra")
    sink.assert_called_once_with(r.bugs, r.jira, "extra")
    assert out == "ok"


# ---------------------------------------------------------------------------
# perform_* operations
# ---------------------------------------------------------------------------


def test_perform_get_invokes_sftp_get(tmp_path: Path) -> None:
    r = _make(tmp_path)
    # path required by report_wd
    r.path = tmp_path / "fake.tpl"
    r.path.write_text("hi")
    targets = MagicMock()
    r.perform_get(targets, Path("/remote/file.txt"))  # ty: ignore[invalid-argument-type]
    targets.sftp_get.assert_called_once()


def test_perform_prepare_delegates_with_package_list(tmp_path: Path) -> None:
    r = _make(tmp_path)
    r.packages = {"v": ["bash"]}
    targets = MagicMock()
    r.perform_prepare(targets, force=True)  # ty: ignore[invalid-argument-type]
    targets.perform_prepare.assert_called_once()
    args, kwargs = targets.perform_prepare.call_args
    assert "bash" in args[0]
    assert args[1] is r
    assert kwargs["force"] is True


def test_perform_install_records_history_and_delegates(tmp_path: Path) -> None:
    r = _make(tmp_path)
    targets = MagicMock()
    r.perform_install(targets, ["bash"])  # ty: ignore[invalid-argument-type]
    targets.add_history.assert_called_once()
    targets.perform_install.assert_called_once_with(["bash"])


def test_perform_uninstall_records_history_and_delegates(tmp_path: Path) -> None:
    r = _make(tmp_path)
    targets = MagicMock()
    r.perform_uninstall(targets, ["bash"])  # ty: ignore[invalid-argument-type]
    targets.add_history.assert_called_once()
    targets.perform_uninstall.assert_called_once_with(["bash"])


def test_perform_downgrade_records_history_and_delegates(tmp_path: Path) -> None:
    r = _make(tmp_path)
    r.packages = {"v": ["bash"]}
    targets = MagicMock()
    r.perform_downgrade(targets)
    targets.add_history.assert_called_once()
    targets.perform_downgrade.assert_called_once()


def test_perform_update_happy_path(tmp_path: Path) -> None:
    r = _make(tmp_path)
    r.packages = {"v": ["bash"]}
    targets = MagicMock()
    r.perform_update(targets, ["--quiet"])  # ty: ignore[invalid-argument-type]
    targets.add_history.assert_called_once()
    targets.perform_update.assert_called_once_with(r, ["--quiet"])


def test_perform_update_rolls_back_then_reraises_on_update_error(
    tmp_path: Path,
) -> None:
    r = _make(tmp_path)
    r.packages = {"v": ["bash"]}
    targets = MagicMock()
    targets.perform_update.side_effect = UpdateError("boom", "h")
    # The update error is surfaced to the caller even though we roll back.
    with pytest.raises(UpdateError, match="boom"):
        r.perform_update(targets, [])  # ty: ignore[invalid-argument-type]
    # add_history called twice: once for update, once for the rollback
    assert targets.add_history.call_count == 2
    targets.perform_downgrade.assert_called_once()


def test_perform_update_reraises_update_error_even_if_rollback_fails(
    tmp_path: Path,
) -> None:
    """A failed rollback must not mask the original update error."""
    r = _make(tmp_path)
    r.packages = {"v": ["bash"]}
    targets = MagicMock()
    targets.perform_update.side_effect = UpdateError("dependency error", "h1")
    # Rollback blows up (e.g. the historical KeyError) — the caller must still
    # see the original UpdateError, not the rollback exception.
    targets.perform_downgrade.side_effect = KeyError("h2")
    with pytest.raises(UpdateError, match="dependency error"):
        r.perform_update(targets, [])  # ty: ignore[invalid-argument-type]
    targets.perform_downgrade.assert_called_once()


# ---------------------------------------------------------------------------
# report_results
# ---------------------------------------------------------------------------


def test_report_results_returns_target_meta_list(tmp_path: Path) -> None:
    r = _make(tmp_path)
    t = MagicMock()
    t.hostname = "h1"
    t.system = "sles15"
    t.packages = {"bash": MagicMock()}
    t.out = []
    out = r.report_results([t])  # ty: ignore[invalid-argument-type]
    assert len(out) == 1
    assert out[0].hostname == "h1"
    assert out[0].system == "sles15"


# ---------------------------------------------------------------------------
# connect_target
# ---------------------------------------------------------------------------


def test_connect_target_happy_returns_pair(tmp_path: Path) -> None:
    r = _make(tmp_path)
    fake_target = MagicMock()
    fake_target.system = "sles15"
    with patch(
        "mtui.test_reports.testreport.Target", return_value=fake_target
    ) as target_cls:
        t, sys = r.connect_target("h1")
    target_cls.assert_called_once()
    assert t is fake_target
    assert sys == "sles15"


def test_connect_target_exception_returns_false_false(tmp_path: Path) -> None:
    r = _make(tmp_path)
    with patch("mtui.test_reports.testreport.Target", side_effect=RuntimeError("nope")):
        out = r.connect_target("h1")
    assert out == (False, False)


def test_connect_target_noninteractive_when_no_prompter(tmp_path: Path) -> None:
    """No prompter (MCP / headless) -> Target built with interactive=False.

    This is the ``add_host`` path that previously hung: a key-auth failure
    must not drop into an invisible ``getpass`` prompt under MCP.
    """
    r = _make(tmp_path)
    assert r._prompter is None
    fake_target = MagicMock()
    fake_target.system = "sles15"
    with patch(
        "mtui.test_reports.testreport.Target", return_value=fake_target
    ) as target_cls:
        r.connect_target("h1")
    assert target_cls.call_args.kwargs["interactive"] is False


def test_connect_target_interactive_when_prompter_present(tmp_path: Path) -> None:
    """A real prompter (REPL) -> Target built with interactive=True."""
    r = _make(tmp_path)
    r._prompter = MagicMock()
    fake_target = MagicMock()
    fake_target.system = "sles15"
    with patch(
        "mtui.test_reports.testreport.Target", return_value=fake_target
    ) as target_cls:
        r.connect_target("h1")
    assert target_cls.call_args.kwargs["interactive"] is True


# ---------------------------------------------------------------------------
# add_target
# ---------------------------------------------------------------------------


def test_add_target_already_connected_warns_and_returns(tmp_path: Path, caplog) -> None:
    r = _make(tmp_path)
    existing = MagicMock()
    existing.hostname = "h1"
    r.targets["h1"] = existing  # ty: ignore[invalid-assignment]
    with caplog.at_level(logging.WARNING, logger="mtui.template.testreport"):
        r.add_target("h1")
    assert any("already connected" in rec.message for rec in caplog.records)


def test_add_target_noninteractive_when_no_prompter(tmp_path: Path) -> None:
    """``add_host -t <host>`` under MCP builds the Target with interactive=False.

    ``AddHost.__call__`` dispatches to ``TestReport.add_target`` for each
    ``--target``; with no prompter the connection must skip the password
    prompt instead of blocking (the homer.qam.suse.cz hang).
    """
    r = _make(tmp_path)
    assert r._prompter is None
    fake_target = MagicMock()
    fake_target.system = "sles15"
    with patch(
        "mtui.test_reports.testreport.Target", return_value=fake_target
    ) as target_cls:
        r.add_target("h1")
    assert target_cls.call_args.kwargs["interactive"] is False


# ---------------------------------------------------------------------------
# refhosts_from_tp
# ---------------------------------------------------------------------------


def test_refhosts_from_tp_resolves_via_factory(tmp_path: Path) -> None:
    r = _make(tmp_path)
    fake_refhosts = MagicMock()
    fake_refhosts.search.return_value = ["host-a", "host-b"]
    r.refhostsFactory = MagicMock(return_value=fake_refhosts)
    with patch(
        "mtui.test_reports.testreport.Attributes.from_testplatform",
        return_value="attrs",
    ):
        r.refhosts_from_tp("tp-foo")
    assert "host-a" in r.hostnames
    assert "host-b" in r.hostnames


def test_refhosts_from_tp_failed_resolve_swallows(tmp_path: Path) -> None:
    from mtui.hosts.refhost import RefhostsResolveFailedError

    r = _make(tmp_path)
    r.refhostsFactory = MagicMock(side_effect=RefhostsResolveFailedError("bad"))
    # Should not raise
    r.refhosts_from_tp("tp")


# ---------------------------------------------------------------------------
# trivial pytest sanity for shutil-side metadata; never raises.
# ---------------------------------------------------------------------------


def test_repr_includes_class_and_id(tmp_path: Path) -> None:
    r = _make(tmp_path)
    assert "OBSTestReport" in repr(r)
    assert str(r.id) in repr(r)


def test_init_default_attrs_present(tmp_path: Path) -> None:
    r = _make(tmp_path)
    # _attrs always has the base set
    for a in ("category", "packager", "reviewer", "packages", "bugs", "repository"):
        assert a in r._attrs


@pytest.mark.parametrize(
    "missing",
    [
        ("category",),
        ("packager",),
    ],
)
def test_warn_missing_fields_partial_blanks(tmp_path: Path, caplog, missing) -> None:
    r = _make(tmp_path)
    # Fill everything except the listed missing.
    for a in r._attrs:
        if a in missing:
            continue
        current = getattr(r, a)
        if isinstance(current, dict):
            setattr(r, a, {"k": "v"})
        elif isinstance(current, list):
            setattr(r, a, ["x"])
        else:
            setattr(r, a, "filled")
    with caplog.at_level(logging.WARNING, logger="mtui.template.testreport"):
        r._warn_missing_fields()
    assert any("missing fields" in rec.message for rec in caplog.records)


# ---------------------------------------------------------------------------
# connect_targets — empty hosts branch
# ---------------------------------------------------------------------------


def test_connect_targets_no_hosts_logs_and_clears(tmp_path: Path, caplog) -> None:
    r = _make(tmp_path)
    # No hostnames to connect.
    with caplog.at_level(logging.INFO, logger="mtui.template.testreport"):
        r.connect_targets()
    assert any("No refhosts to add" in rec.message for rec in caplog.records)


def test_connect_targets_drops_inactive_existing_targets(tmp_path: Path) -> None:
    r = _make(tmp_path)
    inactive = MagicMock()
    inactive.connection.is_active.return_value = False
    r.targets["dead"] = inactive  # ty: ignore[invalid-assignment]
    r.connect_targets()
    assert "dead" not in r.targets


# ---------------------------------------------------------------------------
# add_target happy path
# ---------------------------------------------------------------------------


def test_add_target_happy_path_records_system(tmp_path: Path) -> None:
    r = _make(tmp_path)
    fake_target = MagicMock()
    fake_target.system = "sles15-x86"
    with patch("mtui.test_reports.testreport.Target", return_value=fake_target):
        r.add_target("h-new")
    # The Target instance was stored and the system string was recorded.
    assert r.targets["h-new"] is fake_target
    assert r.systems["h-new"] == "sles15-x86"


def test_add_target_exception_cleans_up(tmp_path: Path) -> None:
    r = _make(tmp_path)
    # Target() raises; nothing should remain in targets/systems
    with patch("mtui.test_reports.testreport.Target", side_effect=RuntimeError("nope")):
        r.add_target("h-bad")
    assert "h-bad" not in r.targets


# ---------------------------------------------------------------------------
# list_versions
# ---------------------------------------------------------------------------


def test_list_versions_aggregates_by_host_then_package(tmp_path: Path) -> None:
    r = _make(tmp_path)
    targets = MagicMock()
    t = MagicMock()
    t.lastout.return_value = "bash 5.1\nopenssl 3.0"
    targets.items.return_value = [("h1", t)]
    sink = MagicMock(return_value="ok")
    out = r.list_versions(sink, targets, ["bash"])  # ty: ignore[invalid-argument-type]
    assert out == "ok"
    sink.assert_called_once()
    targets.run.assert_called_once()


# ---------------------------------------------------------------------------
# scripts_wd + report_wd require a path
# ---------------------------------------------------------------------------


def test_scripts_wd_joins_under_report_wd(tmp_path: Path) -> None:
    r = _make(tmp_path)
    r.path = tmp_path / "fake.tpl"
    r.path.write_text("hi")
    out = r.scripts_wd("compare")
    assert str(out).endswith("scripts/compare")
