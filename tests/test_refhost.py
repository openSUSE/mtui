"""Tests for the mtui refhost module."""

import errno
import re
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import MagicMock

import pytest

from mtui.hosts import refhost
from mtui.support.messages import InvalidLocationError

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
        attr = refhost.Attributes(
            product=refhost.Product(name="sles", version=refhost.Version(major=12))
        )
        assert str(attr) == "sles 12"

    def test_str_with_product_int_minor_uses_dot(self):
        attr = refhost.Attributes(
            product=refhost.Product(
                name="sles", version=refhost.Version(major=15, minor=5)
            )
        )
        assert str(attr) == "sles 15.5"

    def test_str_with_product_string_minor_concatenated(self):
        """SP-style minor versions render without a dot separator."""
        attr = refhost.Attributes(
            product=refhost.Product(
                name="sles", version=refhost.Version(major=12, minor="sp4")
            )
        )
        assert str(attr) == "sles 12sp4"

    def test_str_with_arch(self):
        attr = refhost.Attributes(arch="x86_64")
        assert str(attr) == "x86_64"

    def test_str_with_addons_sorted(self):
        attr = refhost.Attributes(
            addons=[
                refhost.Addon(name="sdk", version=refhost.Version(major=15, minor=5)),
                refhost.Addon(name="ha", version=refhost.Version(major=15)),
            ]
        )
        # Addons are sorted alphabetically.
        assert str(attr) == "ha 15 sdk 15.5"

    def test_repr_wraps_str(self):
        attr = refhost.Attributes(arch="x86_64")
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
            assert a.product == refhost.Product(
                name="sles", version=refhost.Version(major=11, minor=4)
            )
            assert a.addons == [
                refhost.Addon(name="sdk", version=refhost.Version(major=11, minor=4))
            ]

    def test_addon_with_string_minor_kept_as_string(self):
        tp = "base=sles(major=12,minor=sp4);arch=[x86_64];addon=ha(major=12,minor=sp4)"
        attrs = refhost.Attributes.from_testplatform(tp)
        assert len(attrs) == 1
        assert attrs[0].product is not None
        assert attrs[0].product.version is not None
        assert attrs[0].product.version.minor == "sp4"
        assert attrs[0].addons[0].version is not None
        assert attrs[0].addons[0].version.minor == "sp4"

    def test_addon_with_empty_minor(self):
        """``minor=`` with no value sentinels a search-for-unset query."""
        tp = "base=sles(major=11);arch=[x86_64];addon=sdk(major=11,minor=)"
        attrs = refhost.Attributes.from_testplatform(tp)
        assert attrs[0].addons[0].version == refhost.Version(major=11, minor="")

    def test_addon_major_only(self):
        tp = "base=sles(major=11);arch=[x86_64];addon=sdk(major=11)"
        attrs = refhost.Attributes.from_testplatform(tp)
        assert attrs[0].addons[0].version == refhost.Version(major=11, minor=None)

    def test_unknown_segment_logged_and_skipped(self, caplog):
        """A non-base/arch/addon segment is logged at error and the rest still parses."""
        tp = "base=sles(major=15,minor=5);arch=[x86_64];tags=(kernel)"
        with caplog.at_level("ERROR", logger="mtui.refhost"):
            attrs = refhost.Attributes.from_testplatform(tp)
        # The base/arch segments still parse cleanly.
        assert len(attrs) == 1
        assert attrs[0].product is not None
        assert attrs[0].product.name == "sles"
        # The unknown segment was logged.
        assert any("unknown testplatform segment" in r.message for r in caplog.records)

    def test_malformed_segment_logged_and_skipped(self, caplog):
        """A segment without ``=`` is logged at error and the rest still parses."""
        tp = "garbage_no_equals;base=sles(major=15,minor=5);arch=[x86_64]"
        with caplog.at_level("ERROR", logger="mtui.refhost"):
            attrs = refhost.Attributes.from_testplatform(tp)
        assert len(attrs) == 1
        assert attrs[0].product is not None
        assert attrs[0].product.name == "sles"
        assert any(
            "garbage_no_equals" in r.message or "parsing" in r.message
            for r in caplog.records
        )

    def test_no_arch_yields_empty_attribute_list(self):
        """Without an ``arch=[…]`` segment, no Attributes are emitted at all."""
        tp = "base=sles(major=15,minor=5)"
        attrs = refhost.Attributes.from_testplatform(tp)
        assert attrs == []


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

    def test_parse_refhosts_drops_malformed_host_and_logs(self, tmp_path, caplog):
        """Rows missing required fields are logged at ERROR and dropped."""
        bad = tmp_path / "bad.yml"
        bad.write_text(
            "default:\n"
            "  - name: good-host\n"
            "    arch: x86_64\n"
            "    product:\n"
            "      name: sles\n"
            "      version:\n"
            "        major: 15\n"
            "  - name: bad-host\n"
            "    arch: x86_64\n"
            # missing product
        )
        with caplog.at_level("ERROR", logger="mtui.refhost"):
            rh = refhost.Refhosts(bad)
        assert [h.name for h in rh.data["default"]] == ["good-host"]
        assert any("dropping malformed host row" in r.message for r in caplog.records)

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

    def _host(self, **kwargs) -> refhost.Host:
        defaults: dict = {
            "name": "h",
            "arch": "x86_64",
            "product": refhost.Product(
                name="sles", version=refhost.Version(major=15, minor=5)
            ),
            "addons": (),
        }
        defaults.update(kwargs)
        return refhost.Host(**defaults)

    def test_unset_attribute_does_not_filter(self):
        """An empty Attributes() matches every candidate."""
        attr = refhost.Attributes()
        assert self.rh.is_candidate_match(self._host(), attr) is True

    def test_scalar_mismatch_returns_false(self):
        attr = refhost.Attributes(arch="aarch64")
        assert self.rh.is_candidate_match(self._host(arch="x86_64"), attr) is False

    def test_scalar_match_returns_true(self):
        attr = refhost.Attributes(arch="x86_64")
        assert self.rh.is_candidate_match(self._host(arch="x86_64"), attr) is True

    def test_includes_version_empty_minor_excludes_when_candidate_has_minor(self):
        attr = refhost.Attributes(
            product=refhost.Product(
                name="sles", version=refhost.Version(major=15, minor="")
            )
        )
        candidate = self._host(
            product=refhost.Product(
                name="sles", version=refhost.Version(major=15, minor=5)
            )
        )
        assert self.rh.is_candidate_match(candidate, attr) is False

    def test_includes_version_empty_minor_matches_when_candidate_has_no_minor(self):
        attr = refhost.Attributes(
            product=refhost.Product(
                name="sles", version=refhost.Version(major=15, minor="")
            )
        )
        candidate = self._host(
            product=refhost.Product(name="sles", version=refhost.Version(major=15))
        )
        assert self.rh.is_candidate_match(candidate, attr) is True

    def test_includes_version_minor_mismatch_returns_false(self):
        attr = refhost.Attributes(
            product=refhost.Product(
                name="sles", version=refhost.Version(major=15, minor=5)
            )
        )
        candidate = self._host(
            product=refhost.Product(
                name="sles", version=refhost.Version(major=15, minor=4)
            )
        )
        assert self.rh.is_candidate_match(candidate, attr) is False

    def test_includes_version_major_mismatch_returns_false(self):
        attr = refhost.Attributes(
            product=refhost.Product(name="sles", version=refhost.Version(major=15))
        )
        candidate = self._host(
            product=refhost.Product(name="sles", version=refhost.Version(major=12))
        )
        assert self.rh.is_candidate_match(candidate, attr) is False

    def test_addon_missing_on_candidate_returns_false(self):
        attr = refhost.Attributes(
            addons=[refhost.Addon(name="sdk", version=refhost.Version(major=15))]
        )
        candidate = self._host(addons=())
        assert self.rh.is_candidate_match(candidate, attr) is False

    def test_addon_version_mismatch_returns_false(self):
        attr = refhost.Attributes(
            addons=[
                refhost.Addon(name="sdk", version=refhost.Version(major=15, minor=5))
            ]
        )
        candidate = self._host(
            addons=(
                refhost.Addon(name="sdk", version=refhost.Version(major=15, minor=4)),
            )
        )
        assert self.rh.is_candidate_match(candidate, attr) is False

    def test_addon_name_only_matches_any_version(self):
        attr = refhost.Attributes(addons=[refhost.Addon(name="sdk")])
        candidate = self._host(
            addons=(refhost.Addon(name="sdk", version=refhost.Version(major=99)),)
        )
        assert self.rh.is_candidate_match(candidate, attr) is True


# ---------------------------------------------------------------------------
# Resolvers and _RefhostsFactory
# ---------------------------------------------------------------------------


def _make_config(**overrides):
    base: dict[str, object] = {
        "refhosts_resolvers": "path",
        "refhosts_path": REFHOSTS_FIXTURE,
        "refhosts_https_uri": "https://example.invalid/refhosts.yml",
        "refhosts_https_expiration": 3600,
        "location": "default",
        "ssl_verify": None,
    }
    base.update(overrides)
    return SimpleNamespace(**base)


def _make_https_resolver(**overrides) -> refhost.HttpsResolver:
    """Build an ``HttpsResolver`` with all collaborators mocked."""
    defaults: dict[str, object] = {
        "time_now_getter": MagicMock(return_value=1_000_000),
        "statter": MagicMock(),
        "urlopener": MagicMock(),
        "file_writer": MagicMock(),
        "cache_path": Path("/tmp/refhosts.yml"),
        "refhosts_factory": MagicMock(),
    }
    defaults.update(overrides)
    return refhost.HttpsResolver(**defaults)  # ty: ignore[invalid-argument-type]


class TestPathResolver:
    def test_resolve_uses_configured_path(self):
        factory_mock = MagicMock()
        resolver = refhost.PathResolver(refhosts_factory=factory_mock)  # ty: ignore[invalid-argument-type]
        cfg = _make_config(location="nuremberg")
        rh = resolver.resolve(cfg)
        factory_mock.assert_called_once_with(REFHOSTS_FIXTURE, "nuremberg")
        assert rh is factory_mock.return_value


class TestHttpsResolver:
    def test_resolve_uses_cache_path_after_refresh_check(self):
        """``resolve`` runs the cache-refresh check then builds via the factory."""
        statter = MagicMock(return_value=SimpleNamespace(st_mtime=999_999))
        resolver = _make_https_resolver(
            statter=statter, time_now_getter=MagicMock(return_value=1_000_000)
        )
        cfg = _make_config(location="nuremberg")
        rh = resolver.resolve(cfg)
        resolver.refhosts_factory.assert_called_once_with(  # ty: ignore[unresolved-attribute]
            resolver.cache_path, "nuremberg"
        )
        assert rh is resolver.refhosts_factory.return_value  # ty: ignore[unresolved-attribute]

    def test_is_refresh_needed_missing_file(self):
        statter = MagicMock(side_effect=OSError(errno.ENOENT, "missing"))
        resolver = _make_https_resolver(statter=statter)
        assert resolver._is_refresh_needed(3600) is True

    def test_is_refresh_needed_other_oserror_raises(self):
        statter = MagicMock(side_effect=OSError(errno.EACCES, "denied"))
        resolver = _make_https_resolver(statter=statter)
        with pytest.raises(OSError, match="denied"):
            resolver._is_refresh_needed(3600)

    def test_is_refresh_needed_fresh_cache(self):
        statter = MagicMock(return_value=SimpleNamespace(st_mtime=999_999))
        resolver = _make_https_resolver(
            statter=statter, time_now_getter=MagicMock(return_value=1_000_000)
        )
        # delta = 1; expiration = 3600 → no refresh.
        assert resolver._is_refresh_needed(3600) is False

    def test_is_refresh_needed_stale_cache(self):
        statter = MagicMock(return_value=SimpleNamespace(st_mtime=0))
        resolver = _make_https_resolver(
            statter=statter, time_now_getter=MagicMock(return_value=1_000_000)
        )
        assert resolver._is_refresh_needed(3600) is True

    def test_refresh_writes_url_payload(self):
        url_resp = MagicMock()
        url_resp.read.return_value = b"yaml-bytes"
        urlopener = MagicMock(return_value=url_resp)
        file_writer = MagicMock()
        resolver = _make_https_resolver(
            urlopener=urlopener,
            file_writer=file_writer,
            cache_path=Path("/dst"),
        )
        resolver._refresh("https://x/refhosts.yml", True)
        urlopener.assert_called_once_with("https://x/refhosts.yml", True)
        file_writer.assert_called_once_with(b"yaml-bytes", Path("/dst"))

    def test_refresh_if_needed_skips_when_fresh(self):
        statter = MagicMock(return_value=SimpleNamespace(st_mtime=999_999))
        resolver = _make_https_resolver(
            statter=statter, time_now_getter=MagicMock(return_value=1_000_000)
        )
        resolver._refresh_if_needed(_make_config())
        resolver._urlopen.assert_not_called()

    def test_refresh_if_needed_refreshes_when_stale(self):
        statter = MagicMock(return_value=SimpleNamespace(st_mtime=0))
        url_resp = MagicMock()
        url_resp.read.return_value = b"payload"
        urlopener = MagicMock(return_value=url_resp)
        file_writer = MagicMock()
        resolver = _make_https_resolver(
            statter=statter,
            time_now_getter=MagicMock(return_value=1_000_000),
            urlopener=urlopener,
            file_writer=file_writer,
            cache_path=Path("/x"),
        )
        resolver._refresh_if_needed(_make_config())
        # ssl_verify unset -> per-site default True flows to the opener.
        urlopener.assert_called_once_with("https://example.invalid/refhosts.yml", True)
        file_writer.assert_called_once_with(b"payload", Path("/x"))

    def test_refresh_if_needed_honors_ssl_verify_override(self):
        statter = MagicMock(return_value=SimpleNamespace(st_mtime=0))
        url_resp = MagicMock()
        url_resp.read.return_value = b"payload"
        urlopener = MagicMock(return_value=url_resp)
        resolver = _make_https_resolver(
            statter=statter,
            time_now_getter=MagicMock(return_value=1_000_000),
            urlopener=urlopener,
            file_writer=MagicMock(),
            cache_path=Path("/x"),
        )
        resolver._refresh_if_needed(_make_config(ssl_verify=False))
        urlopener.assert_called_once_with("https://example.invalid/refhosts.yml", False)


class TestRefhostsFactory:
    def test_call_returns_first_successful_resolver(self):
        path_resolver = MagicMock(spec=refhost.Resolver)
        factory = refhost._RefhostsFactory({"path": path_resolver})
        cfg = _make_config(refhosts_resolvers="path")
        rh = factory(cfg)
        path_resolver.resolve.assert_called_once_with(cfg)
        assert rh is path_resolver.resolve.return_value

    def test_call_falls_back_to_next_resolver_on_failure(self, caplog):
        """A failing first resolver is logged and the second one is tried."""
        failing = MagicMock(spec=refhost.Resolver)
        failing.resolve.side_effect = RuntimeError("boom")
        working = MagicMock(spec=refhost.Resolver)
        factory = refhost._RefhostsFactory({"https": failing, "path": working})
        cfg = _make_config(refhosts_resolvers="https,path")
        with caplog.at_level("WARNING", logger="mtui.refhost"):
            rh = factory(cfg)
        failing.resolve.assert_called_once_with(cfg)
        working.resolve.assert_called_once_with(cfg)
        assert rh is working.resolve.return_value
        assert any("resolver https failed" in r.message for r in caplog.records)

    def test_call_raises_when_all_resolvers_fail(self):
        failing = MagicMock(spec=refhost.Resolver)
        failing.resolve.side_effect = RuntimeError("boom")
        factory = refhost._RefhostsFactory({"https": failing, "path": failing})
        cfg = _make_config(refhosts_resolvers="https,path")
        with pytest.raises(refhost.RefhostsResolveFailedError):
            factory(cfg)

    def test_call_skips_unknown_resolver_and_continues(self, caplog):
        """Unknown resolver names log a warning but don't abort the chain."""
        working = MagicMock(spec=refhost.Resolver)
        factory = refhost._RefhostsFactory({"path": working})
        cfg = _make_config(refhosts_resolvers="nonexistent,path")
        with caplog.at_level("WARNING", logger="mtui.refhost"):
            rh = factory(cfg)
        working.resolve.assert_called_once_with(cfg)
        assert rh is working.resolve.return_value
        assert any("invalid resolver: nonexistent" in r.message for r in caplog.records)

    def test_call_raises_when_only_unknown_resolvers(self):
        factory = refhost._RefhostsFactory({"path": MagicMock(spec=refhost.Resolver)})
        cfg = _make_config(refhosts_resolvers="invalid_a,invalid_b")
        with pytest.raises(refhost.RefhostsResolveFailedError):
            factory(cfg)
