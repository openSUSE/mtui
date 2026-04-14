"""Expanded tests for mtui types modules."""

import pytest

from mtui.types import HostLog, Package, Product
from mtui.types.enums import assignment, method
from mtui.types.rpmver import RPMVersion
from mtui.types.systems import System, UnknownSystemError
from mtui.types.urls import URLs

# --- Product ---


class TestProduct:
    def test_creation(self):
        """Test Product NamedTuple creation."""
        p = Product("SLES", "15-SP5", "x86_64")
        assert p.name == "SLES"
        assert p.version == "15-SP5"
        assert p.arch == "x86_64"

    def test_equality(self):
        """Test Product equality."""
        p1 = Product("SLES", "15-SP5", "x86_64")
        p2 = Product("SLES", "15-SP5", "x86_64")
        assert p1 == p2

    def test_inequality(self):
        """Test Product inequality."""
        p1 = Product("SLES", "15-SP5", "x86_64")
        p2 = Product("SLED", "15-SP5", "x86_64")
        assert p1 != p2

    def test_hash(self):
        """Test Product is hashable (for use in sets)."""
        p = Product("SLES", "15-SP5", "x86_64")
        s = {p}
        assert p in s


# --- System ---


class TestSystem:
    def test_init_base_only(self):
        """Test System with base product only."""
        base = Product("SLES", "15-SP5", "x86_64")
        sys = System(base)
        assert sys.get_base() == base
        assert sys.get_addons() == set()

    def test_init_with_addons(self):
        """Test System with base and addons."""
        base = Product("SLES", "15-SP5", "x86_64")
        addon = Product("sle-module-basesystem", "15-SP5", "x86_64")
        sys = System(base, {addon})
        assert addon in sys.get_addons()

    def test_str_without_addons(self):
        """Test System str without addons."""
        base = Product("SLES", "15-SP5", "x86_64")
        sys = System(base)
        result = str(sys)
        assert "sles" in result
        assert "15-SP5" in result
        assert "x86_64" in result
        assert "modules" not in result

    def test_str_with_addons(self):
        """Test System str with addons includes '-modules'."""
        base = Product("SLES", "15-SP5", "x86_64")
        addon = Product("sle-module-basesystem", "15-SP5", "x86_64")
        sys = System(base, {addon})
        result = str(sys)
        assert "modules" in result

    def test_flatten(self):
        """Test flatten returns all products."""
        base = Product("SLES", "15-SP5", "x86_64")
        addon = Product("sle-ha", "15-SP5", "x86_64")
        sys = System(base, {addon})

        flat = sys.flatten()
        assert base in flat
        assert addon in flat
        assert len(flat) == 2

    @pytest.mark.parametrize(
        ("name", "expected"),
        [
            ("SLES", "15"),
            ("SLED", "15"),
            ("SLES_SAP", "15"),
            ("SLE_HPC", "15"),
            ("rhel", "YUM"),
            ("openSUSE", "15"),
            ("SL-Micro", "slmicro"),
        ],
    )
    def test_get_release(self, name, expected):
        """Test get_release for various product names."""
        base = Product(name, "15-SP5", "x86_64")
        sys = System(base)
        assert sys.get_release() == expected

    def test_get_release_unknown_raises(self):
        """Test get_release raises UnknownSystemError for unknown name."""
        base = Product("UnknownOS", "1.0", "x86_64")
        sys = System(base)
        with pytest.raises(UnknownSystemError):
            sys.get_release()

    def test_pretty(self):
        """Test pretty returns formatted product info."""
        base = Product("SLES", "15-SP5", "x86_64")
        addon = Product("sle-ha", "15-SP5", "x86_64")
        sys = System(base, {addon})

        result = sys.pretty()
        assert any("Base product" in line for line in result)
        assert any("Addon" in line for line in result)


# --- Package ---


class TestPackage:
    def test_creation(self):
        """Test Package creation."""
        pkg = Package("bash")
        assert pkg.name == "bash"
        assert pkg.before is None
        assert pkg.after is None
        assert pkg.required is None
        assert pkg.current is None

    def test_str(self):
        """Test Package str."""
        pkg = Package("openssl")
        assert str(pkg) == "openssl"

    def test_repr(self):
        """Test Package repr."""
        pkg = Package("openssl")
        assert "Package" in repr(pkg)
        assert "openssl" in repr(pkg)

    def test_hash(self):
        """Test Package hash is based on name."""
        pkg1 = Package("bash")
        pkg2 = Package("bash")
        assert hash(pkg1) == hash(pkg2)

    def test_version_setters_from_string(self):
        """Test version setters convert strings to RPMVersion."""
        pkg = Package("bash")
        pkg.before = "1.0-1.1"
        pkg.after = "1.0-1.2"
        pkg.required = "1.0-1.2"
        pkg.current = "1.0-1.1"

        assert isinstance(pkg.before, RPMVersion)
        assert isinstance(pkg.after, RPMVersion)
        assert isinstance(pkg.required, RPMVersion)
        assert isinstance(pkg.current, RPMVersion)

    def test_version_setters_from_rpmversion(self):
        """Test version setters accept RPMVersion directly."""
        pkg = Package("bash")
        ver = RPMVersion("1.0-1.1")
        pkg.before = ver
        assert pkg.before is ver

    def test_version_setters_none(self):
        """Test version setters accept None."""
        pkg = Package("bash")
        pkg.before = "1.0-1.1"
        pkg.before = None
        assert pkg.before is None


# --- HostLog ---


class TestHostLog:
    def test_empty(self):
        """Test empty HostLog."""
        hl = HostLog()
        assert len(hl) == 0

    def test_append(self):
        """Test appending a log entry."""
        hl = HostLog()
        hl.append(["ls", "output", "err", 0, 1])
        assert len(hl) == 1
        assert hl[0].command == "ls"
        assert hl[0].stdout == "output"
        assert hl[0].stderr == "err"
        assert hl[0].exitcode == 0
        assert hl[0].runtime == 1

    def test_append_wrong_count_raises(self):
        """Test appending with wrong number of items raises."""
        hl = HostLog()
        with pytest.raises(ValueError, match="it need 5 args"):
            hl.append(["too", "few"])

    def test_append_bytes_conversion(self):
        """Test appending converts bytes to strings."""
        hl = HostLog()
        hl.append([b"ls", b"output", b"err", 0, 1])
        assert isinstance(hl[0].command, str)
        assert isinstance(hl[0].stdout, str)


# --- Enums ---


class TestEnums:
    def test_method_enum(self):
        """Test method enum values."""
        assert method.GET == "get"
        assert method.POST == "post"
        assert method.PATCH == "patch"
        assert method.DELETE == "delete"

    def test_assignment_enum(self):
        """Test assignment enum has expected members."""
        assert hasattr(assignment, "ASSIGNED_USER")
        assert hasattr(assignment, "UNASSIGNED")
        assert hasattr(assignment, "ASSIGNED_OTHER")

    def test_assignment_values_distinct(self):
        """Test assignment enum values are distinct."""
        vals = {
            assignment.ASSIGNED_USER,
            assignment.UNASSIGNED,
            assignment.ASSIGNED_OTHER,
        }
        assert len(vals) == 3


# --- URLs ---


class TestURLs:
    def test_creation(self):
        """Test URLs NamedTuple creation."""
        url = URLs("SLES", "x86_64", "15-SP5", "https://example.com/logs")
        assert url.distri == "SLES"
        assert url.arch == "x86_64"
        assert url.version == "15-SP5"
        assert url.url == "https://example.com/logs"
