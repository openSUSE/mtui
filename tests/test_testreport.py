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
# Workflow mode is per-report (previously global config.auto / config.kernel)
# ---------------------------------------------------------------------------


def test_workflow_mode_defaults_to_manual(tmp_path: Path) -> None:
    """A freshly built report's workflow defaults to manual."""
    from mtui.types import Workflow

    r = _make(tmp_path)
    assert r.workflow is Workflow.MANUAL


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
    r._autolock_new_target(target)
    target.lock.assert_called_once_with("testing of SUSE:PI:34556:1")


def test_autolock_new_target_noop_without_comment(tmp_path: Path) -> None:
    r = _make(tmp_path)
    assert r.lock_comment == ""
    target = MagicMock()
    r._autolock_new_target(target)
    target.lock.assert_not_called()


def test_autolock_new_target_suppresses_foreign_lock(tmp_path: Path) -> None:
    from mtui.hosts.target import TargetLockedError

    r = _make(tmp_path)
    r.lock_comment = "testing of SUSE:PI:34556:1"
    target = MagicMock()
    target.lock.side_effect = TargetLockedError("locked by someone else")
    # A host already locked by another user must not abort the connect flow.
    r._autolock_new_target(target)
    target.lock.assert_called_once()


def test_get_package_list_empty(tmp_path: Path) -> None:
    r = _make(tmp_path)
    r.packages = {}
    assert r.get_package_list() == []


# ---------------------------------------------------------------------------
# Host-arbitration pool selection (RFC §5.7)
# ---------------------------------------------------------------------------


class _FakeRefhosts:
    """Minimal refhosts store returning fixed (host, slot) pool candidates."""

    def __init__(self, pairs):
        self._pairs = pairs

    def search(self, _attributes):  # legacy path (should be unused when pooling)
        return [h.name for h, _slot in self._pairs]

    def search_pool(self, _attributes):
        return self._pairs

    def search_pool_by_query(self, _attributes):
        # The fake's pairs are already tagged with the (query) slot the test
        # wants, so the query-keyed pool search returns them unchanged.
        return self._pairs


def _ph(name, slot):
    h = MagicMock()
    h.name = name
    return (h, slot)


def _pool_report(tmp_path, pairs, *, owner=("reg", "RRID")):
    from mtui.hosts.host_arbiter import HostArbiter

    r = _make(tmp_path)
    r.config.lock_wait = 0
    r.config.lock_wait_poll = 15
    r._arbiter = HostArbiter()
    r._owner = owner
    r.refhostsFactory = MagicMock(return_value=_FakeRefhosts(pairs))
    return r


def test_pool_inactive_without_arbiter_uses_legacy_search(tmp_path: Path) -> None:
    # No arbiter/owner wired up → fall back to the legacy search() path.
    r = _make(tmp_path)
    r._arbiter = None
    r._owner = None
    r.refhostsFactory = MagicMock(
        return_value=_FakeRefhosts([_ph("h1", ("sles", "15", "x86_64", ()))])
    )
    assert r._pool_selection_active() is False
    r.refhosts_from_tp("base=sles(major=15,minor=5);arch=[x86_64]")
    assert r.hostnames == {"h1"}
    assert r._pool_claims == set()


def test_pool_selects_one_host_per_slot(tmp_path: Path) -> None:
    pairs = [
        _ph("a-x86", ("sles", "15-5", "x86_64", ())),
        _ph("b-x86", ("sles", "15-5", "x86_64", ())),  # same slot as a-x86
        _ph("c-arm", ("sles", "15-5", "aarch64", ())),
    ]
    r = _pool_report(tmp_path, pairs)
    r.refhosts_from_tp("tp")
    # one host per distinct slot (2 slots), claimed in the arbiter
    assert len(r._pool_claims) == 2
    assert r.hostnames == r._pool_claims
    for h in r._pool_claims:
        assert r._arbiter.owner_of(h) == r._owner


def test_pool_two_reports_draw_distinct_hosts(tmp_path: Path) -> None:
    from mtui.hosts.host_arbiter import HostArbiter

    arb = HostArbiter()
    pairs = [
        _ph("a-x86", ("sles", "15-5", "x86_64", ())),
        _ph("b-x86", ("sles", "15-5", "x86_64", ())),
    ]
    r1 = _pool_report(tmp_path, pairs, owner=("reg", "RRID1"))
    r2 = _pool_report(tmp_path, pairs, owner=("reg", "RRID2"))
    r1._arbiter = r2._arbiter = arb
    r1.refhosts_from_tp("tp")
    r2.refhosts_from_tp("tp")
    assert r1._pool_claims
    assert r2._pool_claims
    assert r1._pool_claims.isdisjoint(r2._pool_claims)


def test_loading_second_report_keeps_first_reports_hosts(tmp_path: Path) -> None:
    """A freshly selected report must not disturb an existing report's hosts.

    Reproduces the ``load_template -a RRID1`` -> ``load_template -a RRID2``
    regression: RRID2 selecting its own pool hosts must leave RRID1's claims
    and arbiter ownership untouched (each template owns its own hosts).
    """
    from mtui.hosts.host_arbiter import HostArbiter

    arb = HostArbiter()
    pairs = [
        _ph("a-x86", ("sles", "15-5", "x86_64", ())),
        _ph("b-x86", ("sles", "15-5", "x86_64", ())),
    ]
    r1 = _pool_report(tmp_path, pairs, owner=("reg", "RRID1"))
    r1._arbiter = arb
    r1.refhosts_from_tp("tp")
    r1_claims = set(r1._pool_claims)
    assert r1_claims

    # Second template loads later, sharing the process-global arbiter.
    r2 = _pool_report(tmp_path, pairs, owner=("reg", "RRID2"))
    r2._arbiter = arb
    r2.refhosts_from_tp("tp")

    # RRID1's claims and arbiter ownership are unchanged.
    assert r1._pool_claims == r1_claims
    for h in r1_claims:
        assert arb.owner_of(h) == ("reg", "RRID1")
    # RRID2 picked its own, disjoint hosts.
    assert r2._pool_claims.isdisjoint(r1_claims)


def test_release_pool_claims_unlocks_and_releases(tmp_path: Path) -> None:
    pairs = [_ph("a-x86", ("sles", "15-5", "x86_64", ()))]
    r = _pool_report(tmp_path, pairs)
    r.refhosts_from_tp("tp")
    claimed = next(iter(r._pool_claims))
    target = MagicMock()
    r.targets[claimed] = target
    r.release_pool_claims()
    target.pool_unlock.assert_called_once()
    assert r._pool_claims == set()
    assert r._slot_candidates == {}
    assert r._arbiter.owner_of(claimed) is None


def test_pool_records_slot_candidates_for_backup(tmp_path: Path) -> None:
    slot = ("sles", "15-5", "x86_64", ())
    pairs = [_ph("a-x86", slot), _ph("b-x86", slot)]
    r = _pool_report(tmp_path, pairs)
    r.refhosts_from_tp("tp")
    assert sorted(r._slot_candidates[slot]) == ["a-x86", "b-x86"]


def test_pool_selection_ignores_preloaded_template_hostnames(tmp_path: Path) -> None:
    """Pool selection connects one host per slot, not the template's full list.

    In automatic mode the template parser pre-fills ``hostnames`` with every
    ``reference host:`` line — multiple candidates per arch/slot. When the user
    then runs ``add_host`` (no ``--target``) the testplatform/pool path must
    connect exactly one arbiter-chosen host per slot and must NOT drag the
    pre-loaded duplicates along (the regression that connected every candidate).
    """
    slot_x = ("sles", "15-5", "x86_64", ())
    slot_a = ("sles", "15-5", "aarch64", ())
    pairs = [
        _ph("a-x86", slot_x),
        _ph("b-x86", slot_x),  # same slot as a-x86
        _ph("c-arm", slot_a),
        _ph("d-arm", slot_a),  # same slot as c-arm
    ]
    r = _pool_report(tmp_path, pairs)
    # Template autoconnect already loaded every candidate.
    r.hostnames = {"a-x86", "b-x86", "c-arm", "d-arm"}
    _stub_connect(r, {"a-x86", "b-x86", "c-arm", "d-arm"})

    r.refhosts_from_tp("tp")
    r.connect_targets()

    # Exactly one connected host per slot, drawn from the pool claims.
    assert len(r.targets) == 2
    assert set(r.targets) == r._pool_claims
    # One host from each slot, never two of the same slot.
    assert len({h for h in r.targets if h.endswith("x86")}) == 1
    assert len({h for h in r.targets if h.endswith("arm")}) == 1


def test_pool_collapses_same_query_slot_across_installed_addons(
    tmp_path: Path,
) -> None:
    """Same arch/base hosts with differing installed modules share one slot.

    ``search_pool_by_query`` tags interchangeable hosts (same requested
    product/arch/addons) with one shared slot, so the pool draws a single host
    even when each candidate has a different installed-module set -- the real
    cause of the multiple-hosts-per-arch regression.
    """
    # Both x86_64 sles 15-SP7 candidates, one query slot (no requested addons).
    slot = ("SLES", "15-SP7", "x86_64", ())
    pairs = [_ph("host-a", slot), _ph("host-b", slot)]
    r = _pool_report(tmp_path, pairs)
    # Automatic mode pre-loaded both into hostnames.
    r.hostnames = {"host-a", "host-b"}
    _stub_connect(r, {"host-a", "host-b"})

    r.refhosts_from_tp("tp")
    r.connect_targets()

    assert len(r.targets) == 1
    assert set(r.targets) <= {"host-a", "host-b"}
    assert set(r.targets) == r._pool_claims


def test_pool_select_shuffles_candidates(tmp_path: Path) -> None:
    """Per-slot candidates are shuffled so the chosen host is random."""
    slot = ("sles", "15-5", "x86_64", ())
    pairs = [_ph(f"h{i}", slot) for i in range(5)]
    r = _pool_report(tmp_path, pairs)
    with patch("mtui.test_reports.testreport.random.shuffle") as shuffle:
        r.refhosts_from_tp("tp")
    shuffle.assert_called_once()


def test_pool_select_warns_when_slot_exhausted(tmp_path: Path, caplog) -> None:
    """All candidates in a slot already held → warn and claim nothing."""
    slot = ("sles", "15-5", "x86_64", ())
    pairs = [_ph("a-x86", slot), _ph("b-x86", slot)]
    r = _pool_report(tmp_path, pairs)
    # Another owner already holds every candidate for the slot.
    other = ("reg", "OTHER")
    assert r._arbiter.try_acquire("a-x86", other)
    assert r._arbiter.try_acquire("b-x86", other)
    with caplog.at_level(logging.WARNING, logger="mtui.template.testreport"):
        r.refhosts_from_tp("tp")
    assert r._pool_claims == set()
    assert any("no free pool host for slot" in rec.message for rec in caplog.records)


# ---------------------------------------------------------------------------
# Deferred autoconnect (runs after the arbiter is wired by the registry)
# ---------------------------------------------------------------------------


def test_autoconnect_noop_when_not_pending(tmp_path: Path) -> None:
    r = _make(tmp_path)
    assert r._autoconnect_pending is False
    with patch.object(r, "connect_targets") as ct:
        r.autoconnect()
    ct.assert_not_called()


def test_autoconnect_connects_and_resolves_testplatforms(tmp_path: Path) -> None:
    r = _make(tmp_path)
    r._autoconnect_pending = True
    r.testplatforms = ["tp1", "tp2"]
    with (
        patch.object(r, "connect_targets") as ct,
        patch.object(r, "refhosts_from_tp") as rft,
    ):
        r.autoconnect()
    # connect once for testreport hosts, once after resolving testplatforms
    assert ct.call_count == 2
    assert [c.args[0] for c in rft.call_args_list] == ["tp1", "tp2"]
    # flag cleared so a second call is a no-op
    assert r._autoconnect_pending is False
    with patch.object(r, "connect_targets") as ct2:
        r.autoconnect()
    ct2.assert_not_called()


# ---------------------------------------------------------------------------
# Connect-time backup refhost (RFC §5.7)
# ---------------------------------------------------------------------------


def _stub_connect(r, connectable: set[str]) -> None:
    """Patch connect_target so only hosts in ``connectable`` succeed."""

    def fake(host):
        if host in connectable:
            t = MagicMock()
            t.system = f"sys-{host}"
            return t, str(t.system)
        return False, False

    r.connect_target = MagicMock(side_effect=fake)


def test_connect_targets_falls_back_to_backup_on_primary_failure(
    tmp_path: Path,
) -> None:
    slot = ("sles", "15-5", "x86_64", ())
    pairs = [_ph("primary", slot), _ph("backup", slot)]
    r = _pool_report(tmp_path, pairs)
    r.refhosts_from_tp("tp")
    primary = next(iter(r._pool_claims))
    backup = "backup" if primary == "primary" else "primary"
    # Only the backup is reachable.
    _stub_connect(r, {backup})
    r.connect_targets()
    assert backup in r.targets
    assert primary not in r.targets
    assert backup in r._pool_claims
    assert primary not in r._pool_claims


def test_connect_targets_warns_when_all_slot_candidates_down(
    tmp_path: Path, caplog
) -> None:
    slot = ("sles", "15-5", "x86_64", ())
    pairs = [_ph("h1", slot), _ph("h2", slot)]
    r = _pool_report(tmp_path, pairs)
    r.refhosts_from_tp("tp")
    _stub_connect(r, set())  # nothing reachable
    with caplog.at_level(logging.WARNING, logger="mtui.template.testreport"):
        r.connect_targets()
    assert r.targets == {}
    assert any(
        "no connectable pool host for slot" in rec.message for rec in caplog.records
    )


def test_connect_targets_no_backup_when_primary_connects(tmp_path: Path) -> None:
    slot = ("sles", "15-5", "x86_64", ())
    pairs = [_ph("primary", slot), _ph("backup", slot)]
    r = _pool_report(tmp_path, pairs)
    r.refhosts_from_tp("tp")
    primary = next(iter(r._pool_claims))
    _stub_connect(r, {primary})
    r.connect_targets()
    assert primary in r.targets
    # backup was never attempted (only the primary connect_target call)
    attempted = {c.args[0] for c in r.connect_target.call_args_list}
    assert attempted == {primary}


def test_pool_connect_skips_host_locked_by_another_owner(tmp_path: Path) -> None:
    """The autoconnect/pool path honours the *remote* pool lock.

    A host whose remote lock is held by someone else (``try_claim`` -> False)
    must not be added; the pool falls back to a free sibling in the same slot.
    Exercises the real ``connect_target`` lock check (not the ``connect_target``
    stub), so it guards the load->manual->autoconnect connect flow end to end.
    """
    slot = ("sles", "15-5", "x86_64", ())
    pairs = [_ph("locked-host", slot), _ph("free-host", slot)]
    r = _pool_report(tmp_path, pairs)

    # Force a deterministic primary so the assertion is stable: "locked-host"
    # is chosen first, then must yield to the free sibling.
    with patch(
        "mtui.test_reports.testreport.random.shuffle",
        lambda c: c.sort(reverse=True),
    ):
        r.refhosts_from_tp("tp")
    assert "locked-host" in r._pool_claims

    created: dict[str, MagicMock] = {}

    def fake_target(_config, host, *_a, **_kw):
        t = MagicMock()
        t.hostname = host
        t.system = f"sys-{host}"
        # Remote lock is busy only for the already-locked host.
        t.try_claim.return_value = host != "locked-host"
        t.connection.is_active.return_value = True
        created[host] = t
        return t

    with patch("mtui.test_reports.testreport.Target", side_effect=fake_target):
        r.connect_targets()

    # Locked host was connected-then-released; the free sibling is the one kept.
    assert "free-host" in r.targets
    assert "locked-host" not in r.targets
    assert "free-host" in r._pool_claims
    assert "locked-host" not in r._pool_claims
    # In-process claim on the locked host was released back to the arbiter.
    assert r._arbiter.owner_of("locked-host") is None
    # We actually attempted the remote claim on the locked host.
    created["locked-host"].try_claim.assert_called_once()


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
    r.perform_get(targets, Path("/remote/file.txt"))
    targets.sftp_get.assert_called_once()


def test_perform_prepare_delegates_with_package_list(tmp_path: Path) -> None:
    r = _make(tmp_path)
    r.packages = {"v": ["bash"]}
    targets = MagicMock()
    r.perform_prepare(targets, force=True)
    targets.perform_prepare.assert_called_once()
    args, kwargs = targets.perform_prepare.call_args
    assert "bash" in args[0]
    assert args[1] is r
    assert kwargs["force"] is True


def test_perform_install_records_history_and_delegates(tmp_path: Path) -> None:
    r = _make(tmp_path)
    targets = MagicMock()
    r.perform_install(targets, ["bash"])
    targets.add_history.assert_called_once()
    targets.perform_install.assert_called_once_with(["bash"])


def test_perform_uninstall_records_history_and_delegates(tmp_path: Path) -> None:
    r = _make(tmp_path)
    targets = MagicMock()
    r.perform_uninstall(targets, ["bash"])
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
    r.perform_update(targets, ["--quiet"])
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
        r.perform_update(targets, [])
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
        r.perform_update(targets, [])
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
    out = r.report_results([t])
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

    The flag now governs the command-timeout prompt: under MCP a silent
    command timeout aborts the run rather than waiting for an answer that
    cannot come.
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
    r.targets["h1"] = existing
    with caplog.at_level(logging.WARNING, logger="mtui.template.testreport"):
        r.add_target("h1")
    assert any("already connected" in rec.message for rec in caplog.records)


def test_add_target_noninteractive_when_no_prompter(tmp_path: Path) -> None:
    """``add_host -t <host>`` under MCP builds the Target with interactive=False.

    ``AddHost.__call__`` dispatches to ``TestReport.add_target`` for each
    ``--target``; with no prompter the connection runs non-interactively
    so a silent command timeout aborts instead of blocking.
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
    r.targets["dead"] = inactive
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
    out = r.list_versions(sink, targets, ["bash"])
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
