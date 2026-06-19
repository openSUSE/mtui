"""Tests for ``mtui.hosts.refhost.verify`` (product drift detection).

The normalization rules are grounded on real refhosts across SLE 15/16 and
SL-Micro (see the module docstring): detected version strings ``"16.0"`` /
``"6.1"`` (dotted) and ``"15-SP4"`` (service pack), ``qa`` excluded on both
sides, names compared case-insensitively.
"""

from mtui.hosts.refhost import verify
from mtui.hosts.refhost.models import Addon, Host, Product, Version
from mtui.types import Product as DetectedProduct
from mtui.types.systems import System


def _system(base, addons=(), dangling=False) -> System:
    """Build a detected ``System`` from ``(name, version, arch)`` tuples."""
    return System(
        DetectedProduct(*base),
        {DetectedProduct(*a) for a in addons},
        dangling_base=dangling,
    )


def _host(name="h", arch="x86_64", product=("SLES", 16, 0), addons=()) -> Host:
    """Build a refhosts ``Host`` from ``(name, major, minor)`` tuples."""
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


class TestNormalizeVersion:
    def test_dotted_int_minor(self):
        # SLE 16 / SL-Micro: "16.0" -> Version(16, 0), "6.1" -> Version(6, 1)
        assert verify.normalize_version("16.0") == Version(major=16, minor=0)
        assert verify.normalize_version("6.1") == Version(major=6, minor=1)

    def test_service_pack(self):
        # SLE 12/15: "15-SP4" -> Version(15, "SP4")
        assert verify.normalize_version("15-SP4") == Version(major=15, minor="SP4")

    def test_major_only(self):
        assert verify.normalize_version("15") == Version(major=15, minor=None)

    def test_empty_is_none(self):
        assert verify.normalize_version("") is None
        assert verify.normalize_version("   ") is None


class TestCompareBase:
    def test_sle16_exact_match_no_warnings(self):
        """A correctly-matching SLES 16.0 host (carme) yields no drift."""
        system = _system(("SLES", "16.0", "x86_64"))
        host = _host(product=("SLES", 16, 0), arch="x86_64")
        diff = verify.compare(system, host)
        assert diff.ok
        assert diff.warnings() == []

    def test_slmicro_exact_match_no_warnings(self):
        """SL-Micro 6.1 with its extras addon matches metadata exactly."""
        system = _system(
            ("SL-Micro", "6.1", "x86_64"),
            addons=[("SL-Micro-Extras", "6.1", "x86_64")],
        )
        host = _host(
            product=("SL-Micro", 6, 1),
            arch="x86_64",
            addons=[("SL-Micro-Extras", 6, 1)],
        )
        assert verify.compare(system, host).ok

    def test_base_name_mismatch(self):
        system = _system(("SLED", "15-SP4", "x86_64"))
        host = _host(product=("SLES", 15, "SP4"), arch="x86_64")
        diff = verify.compare(system, host)
        assert not diff.ok
        assert diff.base_mismatch is not None
        assert "name" in diff.base_mismatch

    def test_base_name_casefold_matches(self):
        """Names differing only in case are treated as equal."""
        system = _system(("sles", "16.0", "x86_64"))
        host = _host(product=("SLES", 16, 0), arch="x86_64")
        assert verify.compare(system, host).ok

    def test_base_version_mismatch(self):
        system = _system(("SLES", "15-SP4", "x86_64"))
        host = _host(product=("SLES", 15, "SP3"), arch="x86_64")
        diff = verify.compare(system, host)
        assert not diff.ok
        assert "version" in (diff.base_mismatch or "")

    def test_arch_mismatch(self):
        system = _system(("SLES", "16.0", "x86_64"))
        host = _host(product=("SLES", 16, 0), arch="aarch64")
        diff = verify.compare(system, host)
        assert not diff.ok
        assert "arch" in (diff.base_mismatch or "")


class TestCompareAddons:
    def test_extra_addon_detected(self):
        """bojack-style drift: host has modules absent from metadata."""
        system = _system(
            ("SLE_RT", "15-SP4", "x86_64"),
            addons=[
                ("SLES-LTSS", "15-SP4", "x86_64"),
                ("sle-module-basesystem", "15-SP4", "x86_64"),
                ("sle-module-development-tools", "15-SP4", "x86_64"),
            ],
        )
        host = _host(
            product=("SLE_RT", 15, "SP4"),
            arch="x86_64",
            addons=[
                ("SLES-LTSS", 15, "SP4"),
                ("sle-module-basesystem", 15, "SP4"),
            ],
        )
        diff = verify.compare(system, host)
        assert not diff.ok
        assert diff.extra_addons == ["sle-module-development-tools 15-SP4"]
        assert diff.missing_addons == []

    def test_missing_addon_detected(self):
        system = _system(
            ("SLES", "15-SP4", "x86_64"),
            addons=[("sle-module-basesystem", "15-SP4", "x86_64")],
        )
        host = _host(
            product=("SLES", 15, "SP4"),
            arch="x86_64",
            addons=[
                ("sle-module-basesystem", 15, "SP4"),
                ("SLES-LTSS", 15, "SP4"),
            ],
        )
        diff = verify.compare(system, host)
        assert not diff.ok
        assert diff.missing_addons == ["SLES-LTSS 15-SP4"]
        assert diff.extra_addons == []

    def test_addon_version_mismatch(self):
        system = _system(
            ("SLES", "15-SP4", "x86_64"),
            addons=[("sle-module-basesystem", "15-SP3", "x86_64")],
        )
        host = _host(
            product=("SLES", 15, "SP4"),
            arch="x86_64",
            addons=[("sle-module-basesystem", 15, "SP4")],
        )
        diff = verify.compare(system, host)
        assert not diff.ok
        assert diff.missing_addons == []
        assert diff.extra_addons == []
        assert len(diff.mismatched_addons) == 1
        assert "sle-module-basesystem" in diff.mismatched_addons[0]

    def test_qa_excluded_from_both_sides(self):
        """``qa`` is dropped on the detected side, so metadata's ``qa`` must
        not be reported as missing, and a detected ``qa`` not as extra."""
        # parse_system never reports qa, so the detected addon set omits it.
        system = _system(
            ("SLES", "15-SP4", "x86_64"),
            addons=[("sle-module-basesystem", "15-SP4", "x86_64")],
        )
        # metadata lists qa (as the real refhosts.yml does for many hosts).
        host = _host(
            product=("SLES", 15, "SP4"),
            arch="x86_64",
            addons=[
                ("sle-module-basesystem", 15, "SP4"),
                ("qa", 15, "SP4"),
            ],
        )
        assert verify.compare(system, host).ok

    def test_full_sle15_match_no_warnings(self):
        """antares-style host whose module set matches metadata exactly."""
        modules = [
            "sle-module-basesystem",
            "sle-module-server-applications",
            "sle-module-desktop-applications",
            "sle-module-development-tools",
            "sle-module-web-scripting",
        ]
        system = _system(
            ("SLES", "15-SP4", "aarch64"),
            addons=[(m, "15-SP4", "aarch64") for m in modules]
            + [("SLES-LTSS", "15-SP4", "aarch64")],
        )
        host = _host(
            product=("SLES", 15, "SP4"),
            arch="aarch64",
            addons=[(m, 15, "SP4") for m in modules] + [("SLES-LTSS", 15, "SP4")],
        )
        assert verify.compare(system, host).ok


class TestCompareDangling:
    def test_dangling_base_warns_and_skips_base_check(self):
        """A dangling baseproduct symlink is reported; the (placeholder)
        base name/version are not additionally flagged as a mismatch."""
        system = _system(("SLES", "", "x86_64"), dangling=True)
        host = _host(product=("SLES", 16, 0), arch="x86_64")
        diff = verify.compare(system, host)
        assert not diff.ok
        assert diff.dangling_base
        assert diff.base_mismatch is None
        assert any("dangling" in w for w in diff.warnings())

    def test_dangling_still_checks_arch(self):
        """Arch is still compared even with a dangling base."""
        system = _system(("SLES", "", "x86_64"), dangling=True)
        host = _host(product=("SLES", 16, 0), arch="aarch64")
        diff = verify.compare(system, host)
        assert diff.dangling_base
        assert "arch" in (diff.base_mismatch or "")
