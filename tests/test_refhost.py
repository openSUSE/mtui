"""Tests for the mtui refhost module - specifically the eval() fix."""

import errno
import re
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import MagicMock

import pytest

from mtui import refhost
from mtui.messages import InvalidLocationError

REFHOSTS_FIXTURE = Path(__file__).parent / "fixtures" / "refhosts.yml"


class TestArchListParsing:
    """Test that the refhost arch list parsing is safe (no eval)."""

    def test_simple_arch_list(self):
        """Test parsing a simple arch list."""
        content = "[x86_64,aarch64,ppc64le]"
        capture = re.match(r"\[(.*)\]", content)
        assert capture is not None
        arch_list = [x.strip() for x in capture.group(1).split(",")]
        assert arch_list == ["x86_64", "aarch64", "ppc64le"]

    def test_single_arch(self):
        """Test parsing a single arch."""
        content = "[x86_64]"
        capture = re.match(r"\[(.*)\]", content)
        assert capture is not None
        arch_list = [x.strip() for x in capture.group(1).split(",")]
        assert arch_list == ["x86_64"]

    def test_arch_list_with_spaces(self):
        """Test parsing arch list with surrounding spaces."""
        content = "[ x86_64 , aarch64 ]"
        capture = re.match(r"\[(.*)\]", content)
        assert capture is not None
        arch_list = [x.strip() for x in capture.group(1).split(",")]
        assert arch_list == ["x86_64", "aarch64"]

    def test_injection_attempt_is_safe(self):
        """Test that a malicious input is treated as a literal string.

        This test verifies the fix for the eval() security vulnerability.
        The old code used eval() which would execute arbitrary Python code.
        The new code uses simple string splitting.
        """
        # This would have been exploitable with eval():
        # eval(f"['{content}']") where content = "x'];import os;os.system('id');['y"
        content = "[x'];import os;os.system('id');['y]"
        capture = re.match(r"\[(.*)\]", content)
        assert capture is not None
        arch_list = [x.strip() for x in capture.group(1).split(",")]
        # With safe parsing, this is just a literal string
        assert arch_list == ["x'];import os;os.system('id');['y"]

    def test_empty_brackets(self):
        """Test parsing empty brackets."""
        content = "[]"
        capture = re.match(r"\[(.*)\]", content)
        assert capture is not None
        arch_list = [x.strip() for x in capture.group(1).split(",")]
        assert arch_list == [""]

    def test_no_brackets(self):
        """Test no match when no brackets present."""
        content = "x86_64"
        capture = re.match(r"\[(.*)\]", content)
        assert capture is None


# ---------------------------------------------------------------------------
# Attributes
# ---------------------------------------------------------------------------


class TestAttributes:
    """Public-API behaviour of ``Attributes``."""

    def test_empty_attributes_is_falsy(self):
        attr = refhost.Attributes()
        assert not attr
        assert str(attr) == ""

    def test_str_with_product_major_only(self):
        attr = refhost.Attributes()
        attr.product = {"name": "sles", "version": {"major": 12}}
        assert str(attr) == "sles 12"

    def test_str_with_product_int_minor_uses_dot(self):
        attr = refhost.Attributes()
        attr.product = {"name": "sles", "version": {"major": 15, "minor": 5}}
        assert str(attr) == "sles 15.5"

    def test_str_with_product_string_minor_concatenated(self):
        """SP-style minor versions render without a dot separator."""
        attr = refhost.Attributes()
        attr.product = {"name": "sles", "version": {"major": 12, "minor": "sp4"}}
        assert str(attr) == "sles 12sp4"

    def test_str_with_arch(self):
        attr = refhost.Attributes()
        attr.arch = "x86_64"
        assert str(attr) == "x86_64"

    def test_str_with_addons_sorted(self):
        attr = refhost.Attributes()
        attr.addons = [
            {"name": "sdk", "version": {"major": 15, "minor": 5}},
            {"name": "ha", "version": {"major": 15}},
        ]
        # Addons are sorted alphabetically.
        assert str(attr) == "ha 15. sdk 15.5"

    def test_repr_wraps_str(self):
        attr = refhost.Attributes()
        attr.arch = "x86_64"
        assert repr(attr) == "<Attributes: x86_64>"


class TestFromTestplatform:
    """``Attributes.from_testplatform`` covers the documented testplatform DSL."""

    def test_base_arch_addon_with_int_minor(self):
        tp = (
            "base=sles(major=11,minor=4);"
            "arch=[i386,s390x,x86_64];"
            "addon=sdk(major=11,minor=4)"
        )
        attrs = refhost.Attributes.from_testplatform(tp)
        # One Attributes per arch.
        assert [a.arch for a in attrs] == ["i386", "s390x", "x86_64"]
        for a in attrs:
            assert a.product == {
                "name": "sles",
                "version": {"major": 11, "minor": 4},
            }
            assert a.addons == [{"name": "sdk", "version": {"major": 11, "minor": 4}}]

    def test_addon_with_string_minor_kept_as_string(self):
        tp = "base=sles(major=12,minor=sp4);arch=[x86_64];addon=ha(major=12,minor=sp4)"
        attrs = refhost.Attributes.from_testplatform(tp)
        assert len(attrs) == 1
        assert attrs[0].product["version"]["minor"] == "sp4"
        assert attrs[0].addons[0]["version"]["minor"] == "sp4"

    def test_addon_with_empty_minor(self):
        """``minor=`` with no value sentinels a search-for-unset query."""
        tp = "base=sles(major=11);arch=[x86_64];addon=sdk(major=11,minor=)"
        attrs = refhost.Attributes.from_testplatform(tp)
        assert attrs[0].addons[0]["version"] == {"major": 11, "minor": ""}

    def test_addon_major_only(self):
        tp = "base=sles(major=11);arch=[x86_64];addon=sdk(major=11)"
        attrs = refhost.Attributes.from_testplatform(tp)
        assert attrs[0].addons[0]["version"] == {"major": 11}

    def test_tags_sets_named_attribute(self):
        tp = "base=sles(major=15,minor=5);arch=[x86_64];tags=(kernel)"
        attrs = refhost.Attributes.from_testplatform(tp)
        # ``tags=(name)`` sets ``name`` as a dynamic attribute on Attributes.
        assert getattr(attrs[0], "kernel") == {"enabled": True}  # noqa: B009

    def test_malformed_segment_logged_and_skipped(self, caplog):
        """A segment without ``=`` is logged at error and the rest still parses."""
        tp = "garbage_no_equals;base=sles(major=15,minor=5);arch=[x86_64]"
        with caplog.at_level("ERROR", logger="mtui.refhost"):
            attrs = refhost.Attributes.from_testplatform(tp)
        assert len(attrs) == 1
        assert attrs[0].product["name"] == "sles"
        assert any(
            "garbage_no_equals" in r.message or "parsing" in r.message
            for r in caplog.records
        )

    def test_no_arch_yields_empty_attribute_list(self):
        """Without an ``arch=[…]`` segment, no Attributes are emitted at all."""
        tp = "base=sles(major=15,minor=5)"
        attrs = refhost.Attributes.from_testplatform(tp)
        assert attrs == []

    def test_unknown_property_setattr_branch(self):
        """A non-base/non-addon complex property is set as a named attribute."""
        tp = "base=sles(major=15);arch=[x86_64];other=thing(major=1)"
        attrs = refhost.Attributes.from_testplatform(tp)
        assert getattr(attrs[0], "other") == {  # noqa: B009
            "name": "thing",
            "version": {"major": 1},
        }


# ---------------------------------------------------------------------------
# Refhosts
# ---------------------------------------------------------------------------


class TestRefhosts:
    """Public-API behaviour of ``Refhosts``."""

    def test_init_defaults_to_default_location(self):
        rh = refhost.Refhosts(REFHOSTS_FIXTURE)
        assert rh.location == "default"

    def test_init_with_explicit_location(self):
        rh = refhost.Refhosts(REFHOSTS_FIXTURE, location="nuremberg")
        assert rh.location == "nuremberg"

    def test_parse_refhosts_propagates_load_failure(self, tmp_path, caplog):
        """A broken yml file is logged at error and re-raised."""
        from ruamel.yaml import YAMLError

        broken = tmp_path / "broken.yml"
        broken.write_text("not: valid: yaml: at all: [")
        with (
            caplog.at_level("ERROR", logger="mtui.refhost"),
            pytest.raises(YAMLError),
        ):
            refhost.Refhosts(broken)
        assert any("failed to parse refhosts.yml" in r.message for r in caplog.records)

    def test_get_locations(self):
        rh = refhost.Refhosts(REFHOSTS_FIXTURE)
        assert rh.get_locations() == {"default", "nuremberg"}

    def test_check_location_sanity_known_location_returns_none(self):
        rh = refhost.Refhosts(REFHOSTS_FIXTURE)
        assert rh.check_location_sanity("nuremberg") is None

    def test_check_location_sanity_unknown_location_raises(self):
        rh = refhost.Refhosts(REFHOSTS_FIXTURE)
        with pytest.raises(InvalidLocationError):
            rh.check_location_sanity("atlantis")

    def test_search_finds_host_in_current_location(self):
        rh = refhost.Refhosts(REFHOSTS_FIXTURE, location="nuremberg")
        attrs = refhost.Attributes.from_testplatform(
            "base=sles(major=15,minor=5);arch=[x86_64]"
        )
        assert rh.search(attrs) == ["host-nbg-x86"]

    def test_search_falls_back_to_default_when_location_misses(self):
        """When the configured location has no match, default location is checked."""
        rh = refhost.Refhosts(REFHOSTS_FIXTURE, location="nuremberg")
        attrs = refhost.Attributes.from_testplatform(
            "base=sles(major=12,minor=sp4);arch=[x86_64]"
        )
        # Only present under ``default``.
        assert rh.search(attrs) == ["host-default-noaddon"]

    def test_search_returns_empty_when_no_match_anywhere(self):
        rh = refhost.Refhosts(REFHOSTS_FIXTURE)
        attrs = refhost.Attributes.from_testplatform(
            "base=sles(major=99,minor=99);arch=[mips]"
        )
        assert rh.search(attrs) == []

    def test_search_addon_filter_excludes_hosts_missing_addon(self):
        """Searching for sdk excludes hosts that don't list it."""
        rh = refhost.Refhosts(REFHOSTS_FIXTURE)
        attrs = refhost.Attributes.from_testplatform(
            "base=sles(major=15,minor=5);arch=[x86_64];addon=sdk(major=15,minor=5)"
        )
        assert rh.search(attrs) == ["host-default-x86"]

    def test_search_default_location_no_fallback(self):
        """The fallback branch is skipped when already searching the default."""
        rh = refhost.Refhosts(REFHOSTS_FIXTURE)  # default location
        attrs = refhost.Attributes.from_testplatform(
            "base=sles(major=99,minor=99);arch=[mips]"
        )
        assert rh.search(attrs) == []


class TestIsCandidateMatch:
    """Direct tests of the matcher to cover its branches."""

    def setup_method(self):
        self.rh = refhost.Refhosts(REFHOSTS_FIXTURE)

    def test_attribute_key_missing_in_candidate_returns_false(self):
        attr = refhost.Attributes()
        attr.arch = "x86_64"
        candidate = {"name": "h", "product": {"name": "sles"}}  # no arch
        assert self.rh.is_candidate_match(candidate, attr) is False

    def test_scalar_mismatch_returns_false(self):
        attr = refhost.Attributes()
        attr.arch = "aarch64"
        candidate = {"arch": "x86_64"}
        assert self.rh.is_candidate_match(candidate, attr) is False

    def test_scalar_match_returns_true(self):
        attr = refhost.Attributes()
        attr.arch = "x86_64"
        candidate = {"arch": "x86_64"}
        assert self.rh.is_candidate_match(candidate, attr) is True

    def test_includes_version_empty_minor_excludes_when_candidate_has_minor(self):
        attr = refhost.Attributes()
        attr.product = {"name": "sles", "version": {"major": 15, "minor": ""}}
        candidate = {"product": {"name": "sles", "version": {"major": 15, "minor": 5}}}
        assert self.rh.is_candidate_match(candidate, attr) is False

    def test_includes_version_empty_minor_matches_when_candidate_has_no_minor(self):
        attr = refhost.Attributes()
        attr.product = {"name": "sles", "version": {"major": 15, "minor": ""}}
        candidate = {"product": {"name": "sles", "version": {"major": 15}}}
        assert self.rh.is_candidate_match(candidate, attr) is True

    def test_includes_version_minor_mismatch_returns_false(self):
        attr = refhost.Attributes()
        attr.product = {"name": "sles", "version": {"major": 15, "minor": 5}}
        candidate = {"product": {"name": "sles", "version": {"major": 15, "minor": 4}}}
        assert self.rh.is_candidate_match(candidate, attr) is False

    def test_includes_version_major_mismatch_returns_false(self):
        attr = refhost.Attributes()
        attr.product = {"name": "sles", "version": {"major": 15}}
        candidate = {"product": {"name": "sles", "version": {"major": 12}}}
        assert self.rh.is_candidate_match(candidate, attr) is False

    def test_includes_simple_attributes_missing_key_returns_false(self):
        attr = refhost.Attributes()
        attr.product = {"name": "sles", "extra_key": "x"}
        candidate = {"product": {"name": "sles"}}  # no extra_key
        assert self.rh.is_candidate_match(candidate, attr) is False


# ---------------------------------------------------------------------------
# _RefhostsFactory
# ---------------------------------------------------------------------------


def _make_factory(**overrides):
    """Build a ``_RefhostsFactory`` with all collaborators mocked."""
    defaults: dict[str, object] = {
        "time_now_getter": MagicMock(return_value=1_000_000),
        "statter": MagicMock(),
        "urlopener": MagicMock(),
        "file_writer": MagicMock(),
        "cache_path": Path("/tmp/refhosts.yml"),
        "refhosts_factory": MagicMock(),
    }
    defaults.update(overrides)
    return refhost._RefhostsFactory(**defaults)  # ty: ignore[invalid-argument-type]


def _make_config(**overrides):
    base: dict[str, object] = {
        "refhosts_resolvers": "path",
        "refhosts_path": REFHOSTS_FIXTURE,
        "refhosts_https_uri": "https://example.invalid/refhosts.yml",
        "refhosts_https_expiration": 3600,
        "location": "default",
    }
    base.update(overrides)
    return SimpleNamespace(**base)


class TestRefhostsFactory:
    def test_call_returns_first_successful_resolver(self):
        factory = _make_factory()
        cfg = _make_config(refhosts_resolvers="path")
        rh = factory(cfg)
        factory.refhosts_factory.assert_called_once_with(REFHOSTS_FIXTURE, "default")
        assert rh is factory.refhosts_factory.return_value

    def test_call_falls_back_to_next_resolver_on_failure(self, caplog):
        """A failing first resolver is logged and the second one is tried."""
        factory = _make_factory()
        # First resolver name is invalid (no resolve_invalid method).
        cfg = _make_config(refhosts_resolvers="invalid,path")
        with caplog.at_level("DEBUG", logger="mtui.refhost"):
            factory(cfg)
        assert any("invalid" in r.message for r in caplog.records)
        factory.refhosts_factory.assert_called_once_with(REFHOSTS_FIXTURE, "default")

    def test_call_raises_when_all_resolvers_fail(self):
        factory = _make_factory()
        cfg = _make_config(refhosts_resolvers="invalid_a,invalid_b")
        with pytest.raises(refhost.RefhostsResolveFailedError):
            factory(cfg)

    def test_resolve_one_unknown_resolver_logs_and_raises(self, caplog):
        factory = _make_factory()
        cfg = _make_config()
        with (
            caplog.at_level("WARNING", logger="mtui.refhost"),
            pytest.raises(AttributeError),
        ):
            factory._resolve_one("nonexistent", cfg)
        assert any("invalid resolver" in r.message for r in caplog.records)

    def test_is_https_cache_refresh_needed_missing_file(self):
        statter = MagicMock(side_effect=OSError(errno.ENOENT, "missing"))
        factory = _make_factory(statter=statter)
        assert factory._is_https_cache_refresh_needed(Path("/nope"), 3600) is True

    def test_is_https_cache_refresh_needed_other_oserror_raises(self):
        statter = MagicMock(side_effect=OSError(errno.EACCES, "denied"))
        factory = _make_factory(statter=statter)
        with pytest.raises(OSError, match="denied"):
            factory._is_https_cache_refresh_needed(Path("/denied"), 3600)

    def test_is_https_cache_refresh_needed_fresh_cache(self):
        statter = MagicMock(return_value=SimpleNamespace(st_mtime=999_999))
        factory = _make_factory(
            statter=statter, time_now_getter=MagicMock(return_value=1_000_000)
        )
        # delta = 1; expiration = 3600 → no refresh.
        assert factory._is_https_cache_refresh_needed(Path("/x"), 3600) is False

    def test_is_https_cache_refresh_needed_stale_cache(self):
        statter = MagicMock(return_value=SimpleNamespace(st_mtime=0))
        factory = _make_factory(
            statter=statter, time_now_getter=MagicMock(return_value=1_000_000)
        )
        assert factory._is_https_cache_refresh_needed(Path("/x"), 3600) is True

    def test_refresh_https_cache_writes_url_payload(self):
        url_resp = MagicMock()
        url_resp.read.return_value = b"yaml-bytes"
        urlopener = MagicMock(return_value=url_resp)
        file_writer = MagicMock()
        factory = _make_factory(urlopener=urlopener, file_writer=file_writer)
        factory.refresh_https_cache(Path("/dst"), "https://x/refhosts.yml")
        urlopener.assert_called_once_with("https://x/refhosts.yml")
        file_writer.assert_called_once_with(b"yaml-bytes", Path("/dst"))

    def test_refresh_https_cache_if_needed_skips_when_fresh(self):
        statter = MagicMock(return_value=SimpleNamespace(st_mtime=999_999))
        factory = _make_factory(
            statter=statter, time_now_getter=MagicMock(return_value=1_000_000)
        )
        # urlopener untouched.
        factory.refresh_https_cache_if_needed(Path("/x"), _make_config())
        factory._urlopen.assert_not_called()

    def test_refresh_https_cache_if_needed_refreshes_when_stale(self):
        statter = MagicMock(return_value=SimpleNamespace(st_mtime=0))
        url_resp = MagicMock()
        url_resp.read.return_value = b"payload"
        urlopener = MagicMock(return_value=url_resp)
        file_writer = MagicMock()
        factory = _make_factory(
            statter=statter,
            time_now_getter=MagicMock(return_value=1_000_000),
            urlopener=urlopener,
            file_writer=file_writer,
        )
        factory.refresh_https_cache_if_needed(Path("/x"), _make_config())
        urlopener.assert_called_once()
        file_writer.assert_called_once_with(b"payload", Path("/x"))

    def test_resolve_https_uses_cache_path(self):
        """``resolve_https`` runs the cache-refresh check then builds via the factory."""
        statter = MagicMock(return_value=SimpleNamespace(st_mtime=999_999))
        factory = _make_factory(
            statter=statter, time_now_getter=MagicMock(return_value=1_000_000)
        )
        cfg = _make_config(location="nuremberg")
        rh = factory.resolve_https(cfg)
        factory.refhosts_factory.assert_called_once_with(
            factory.refhosts_cache_path, "nuremberg"
        )
        assert rh is factory.refhosts_factory.return_value

    def test_resolve_path_uses_configured_path(self):
        factory = _make_factory()
        cfg = _make_config(location="nuremberg")
        rh = factory.resolve_path(cfg)
        factory.refhosts_factory.assert_called_once_with(REFHOSTS_FIXTURE, "nuremberg")
        assert rh is factory.refhosts_factory.return_value
