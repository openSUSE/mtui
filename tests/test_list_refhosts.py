"""Tests for the offline refhost query engine (:meth:`Refhosts.query` +
helpers) and the ``list_refhosts`` command's record gathering.

The engine is exercised directly against ``tests/fixtures/refhosts.yml``; the
command's ``_gather`` is driven through a hand-built instance wired to a config
that resolves the same fixture via the ``path`` resolver — no SSH, no Command
harness, no template.
"""

from __future__ import annotations

import io
import json as _json
from pathlib import Path
from types import SimpleNamespace

import pytest

from mtui.commands import list_refhosts as lr
from mtui.commands.list_refhosts import ListRefhosts
from mtui.hosts.refhost.store import Refhosts

FIXTURE = Path(__file__).parent / "fixtures" / "refhosts.yml"


def _rh() -> Refhosts:
    return Refhosts(FIXTURE)


# --------------------------------------------------------------------------- #
# Refhosts.query                                                              #
# --------------------------------------------------------------------------- #
def test_query_no_filter_lists_all_hosts_deduped() -> None:
    names = sorted(h.name for h in _rh().query())
    # every legacy location group merged into one list, de-duplicated by name
    assert names == [
        "host-default-aarch64",
        "host-default-noaddon",
        "host-default-x86",
        "host-nbg-only-here",
        "host-nbg-x86",
    ]


def test_query_arch_filter() -> None:
    names = {h.name for h in _rh().query(arch=["x86_64"])}
    assert names == {"host-default-noaddon", "host-default-x86", "host-nbg-x86"}


def test_query_product_substring_and_version() -> None:
    # product substring is case-insensitive; SP optional in version
    hits = _rh().query(product="sles", version="15-SP5")
    assert {h.name for h in hits} == {
        "host-default-aarch64",
        "host-default-x86",
        "host-nbg-only-here",
        "host-nbg-x86",
    }
    assert _rh().query(product="sles", version="15.5") == hits  # 15.5 == 15-SP5


def test_query_name_glob() -> None:
    assert {h.name for h in _rh().query(name="host-nbg-*")} == {
        "host-nbg-only-here",
        "host-nbg-x86",
    }


def test_query_empty_on_no_match() -> None:
    assert _rh().query(arch=["s390x"], product="sles") == []


def test_query_by_testplatform_attributes() -> None:
    from mtui.hosts.refhost.models import Attributes

    attrs = Attributes.from_testplatform("base=sles(major=15,minor=5);arch=[x86_64]")
    names = {h.name for h in _rh().query(attributes=attrs)}
    assert names == {"host-default-x86", "host-nbg-x86"}


# --------------------------------------------------------------------------- #
# search_pool / slot_of                                                       #
# --------------------------------------------------------------------------- #
def test_search_pool_tags_slots() -> None:
    from mtui.hosts.refhost.models import Attributes

    attrs = Attributes.from_testplatform("base=sles(major=15,minor=5);arch=[x86_64]")
    pairs = _rh().search_pool(attrs)
    # matches both x86 hosts, each tagged with a slot tuple
    by_name = {h.name: slot for h, slot in pairs}
    assert set(by_name) == {"host-default-x86", "host-nbg-x86"}
    for slot in by_name.values():
        assert isinstance(slot, tuple)
        assert len(slot) == 4


def test_slot_of_distinguishes_arch_and_version() -> None:
    rh = _rh()
    hosts = {h.name: h for h in rh.query()}
    x86 = rh.slot_of(hosts["host-default-x86"])
    aarch = rh.slot_of(hosts["host-default-aarch64"])
    # same product/version, different arch -> different slot
    assert x86[:2] == aarch[:2]
    assert x86[2] != aarch[2]


# --------------------------------------------------------------------------- #
# _version_str_match                                                          #
# --------------------------------------------------------------------------- #
def test_version_str_match_variants() -> None:
    from mtui.hosts.refhost.models import Version

    v = Version(15, "SP5")
    assert Refhosts._version_str_match(v, "15-SP5") is True
    assert Refhosts._version_str_match(v, "15.5") is True
    assert Refhosts._version_str_match(v, "15") is True  # bare major matches any minor
    assert Refhosts._version_str_match(v, "15-SP6") is False
    assert Refhosts._version_str_match(v, "12") is False
    assert Refhosts._version_str_match(None, "15") is False  # no version never matches


# --------------------------------------------------------------------------- #
# command _gather (record shape, pool slots, testplatform path)               #
# --------------------------------------------------------------------------- #
def _cfg() -> SimpleNamespace:
    # makes RefhostsFactory(config) resolve the fixture via the 'path' resolver
    return SimpleNamespace(refhosts_resolvers="path", refhosts_path=FIXTURE)


def _cmd(**args) -> ListRefhosts:
    defaults = {
        "testplatform": None,
        "name": None,
        "arch": None,
        "product": None,
        "version": None,
        "addon": None,
        "pool": False,
        "as_json": False,
        "free": False,
        "verbose": False,
    }
    defaults.update(args)
    cmd = ListRefhosts.__new__(ListRefhosts)
    cmd.config = _cfg()
    cmd.args = SimpleNamespace(**defaults)
    cmd.sys = SimpleNamespace(stdout=io.StringIO())
    return cmd


def _out(cmd: ListRefhosts) -> str:
    return cmd.sys.stdout.getvalue()


def test_gather_field_filter_record_shape() -> None:
    recs = _cmd(arch=["x86_64"], product="sles", version="15-SP5")._gather()
    by_name = {r["name"]: r for r in recs}
    assert set(by_name) == {"host-default-x86", "host-nbg-x86"}
    r = by_name["host-nbg-x86"]
    assert r["arch"] == "x86_64"
    assert r["product"] == "sles"  # fixture uses lowercase; real yml uses SLES
    assert r["version"] == "15-5"  # fixture minor is int 5; real yml is SP5
    assert isinstance(r["addons"], list)


def test_gather_pool_assigns_slots() -> None:
    recs = _cmd(arch=["x86_64"], pool=True)._gather()
    assert recs  # non-empty
    assert all(r["slot"] for r in recs)


def test_gather_testplatform_path() -> None:
    recs = _cmd(testplatform="base=sles(major=15,minor=5);arch=[x86_64]")._gather()
    assert {r["name"] for r in recs} == {"host-default-x86", "host-nbg-x86"}


# --------------------------------------------------------------------------- #
# command __call__ output (json / table / pool / empty / --free)              #
# --------------------------------------------------------------------------- #
def test_call_json_emits_record_list() -> None:
    cmd = _cmd(arch=["x86_64"], as_json=True)
    cmd()
    data = _json.loads(_out(cmd))
    assert {r["name"] for r in data} == {
        "host-default-noaddon",
        "host-default-x86",
        "host-nbg-x86",
    }


def test_call_table_lists_hosts_and_count() -> None:
    cmd = _cmd(arch=["aarch64"])
    cmd()
    out = _out(cmd)
    assert "host-default-aarch64" in out
    assert "refhost(s)" in out


def test_call_pool_groups_by_slot() -> None:
    cmd = _cmd(arch=["x86_64"], pool=True)
    cmd()
    assert "== " in _out(cmd)  # at least one slot header


def test_call_no_match_message() -> None:
    cmd = _cmd(arch=["s390x"], product="sles")
    cmd()
    assert "no refhosts match" in _out(cmd)


def test_call_free_probes_lock_state(monkeypatch: pytest.MonkeyPatch) -> None:
    class _FakeTarget:
        def __init__(self, _config, name, interactive: bool = True) -> None:
            self.name = name

        def connect(self) -> None:
            pass

        def is_locked(self) -> bool:
            return "nbg" in self.name

        def close(self) -> None:
            pass

    monkeypatch.setattr(lr, "Target", _FakeTarget)
    cmd = _cmd(arch=["x86_64"], as_json=True, free=True)
    cmd()
    data = {r["name"]: r["lock"] for r in _json.loads(_out(cmd))}
    assert data["host-nbg-x86"] == "locked"
    assert data["host-default-x86"] == "free"
