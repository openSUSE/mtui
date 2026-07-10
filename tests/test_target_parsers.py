"""Tests for the mtui target parsers modules."""

from unittest.mock import MagicMock, patch

from mtui.hosts.target.parsers.product import parse_os_release, parse_product

# --- parse_product ---


class TestParseProduct:
    def test_parse_basic_product(self):
        """Test parsing a basic product XML file."""
        xml = """<?xml version="1.0" encoding="UTF-8"?>
<product>
    <name>SLES</name>
    <baseversion>15</baseversion>
    <patchlevel>5</patchlevel>
    <arch>x86_64</arch>
</product>"""
        mock_file = MagicMock()
        mock_file.__iter__ = lambda self: iter(xml.splitlines(True))

        name, version, arch = parse_product(mock_file)

        assert name == "SLES"
        assert version == "15-SP5"
        assert arch == "x86_64"

    def test_parse_product_no_patchlevel(self):
        """Test parsing product with patchlevel 0 (no SP suffix)."""
        xml = """<?xml version="1.0" encoding="UTF-8"?>
<product>
    <name>SLES</name>
    <baseversion>15</baseversion>
    <patchlevel>0</patchlevel>
    <arch>x86_64</arch>
</product>"""
        mock_file = MagicMock()
        mock_file.__iter__ = lambda self: iter(xml.splitlines(True))

        name, version, arch = parse_product(mock_file)

        assert name == "SLES"
        assert version == "15"
        assert arch == "x86_64"

    def test_parse_product_with_version_only(self):
        """Test parsing product with <version> instead of <baseversion>."""
        xml = """<?xml version="1.0" encoding="UTF-8"?>
<product>
    <name>SL-Micro</name>
    <version>6.0</version>
    <arch>x86_64</arch>
</product>"""
        mock_file = MagicMock()
        mock_file.__iter__ = lambda self: iter(xml.splitlines(True))

        name, version, arch = parse_product(mock_file)

        assert name == "SL-Micro"
        assert version == "6.0"
        assert arch == "x86_64"


# --- parse_os_release ---


class TestParseOsRelease:
    def test_parse_basic_os_release(self):
        """Test parsing a basic os-release file."""
        content = [
            'ID="ubuntu"\n',
            'VERSION_ID="22.04"\n',
            'NAME="Ubuntu"\n',
        ]
        mock_file = MagicMock()
        mock_file.readlines.return_value = content

        name, version, arch = parse_os_release(mock_file)

        assert name == "ubuntu"
        assert version == "22.04"
        assert arch == "x86_64"

    def test_parse_os_release_with_comments(self):
        """Test parsing os-release file with comments."""
        content = [
            "# This is a comment\n",
            'ID="sles"\n',
            "\n",
            'VERSION_ID="15.5"\n',
        ]
        mock_file = MagicMock()
        mock_file.readlines.return_value = content

        name, version, arch = parse_os_release(mock_file)

        assert name == "sles"
        assert version == "15.5"

    def test_parse_os_release_strips_quotes(self):
        """Test that double quotes are stripped from values."""
        content = [
            'ID="rhel"\n',
            'VERSION_ID="8.6"\n',
        ]
        mock_file = MagicMock()
        mock_file.readlines.return_value = content

        name, version, _ = parse_os_release(mock_file)

        assert '"' not in name
        assert '"' not in version


# --- parse_system (integration-style) ---


def _mock_connection_with_sftp() -> tuple[MagicMock, MagicMock]:
    """Return a mock Connection plus the SFTPClient yielded by sftp_session()."""
    conn = MagicMock()
    conn.hostname = "host1"
    sftp = MagicMock()
    conn.sftp_session.return_value.__enter__.return_value = sftp
    return conn, sftp


def _dispatch_open(*, transactional: bool):
    """Build a path-aware ``sftp.open`` side effect.

    Product file opens return a generic context-manager mock (the parsed
    values come from the mocked ``product`` module). Opening a
    ``transactional-update.conf`` path succeeds when ``transactional`` is
    True, else raises ``FileNotFoundError``. Being path-based (not a fixed
    call sequence) keeps the tests robust to how many config locations the
    detector probes.
    """
    product_file = MagicMock()

    def _open(path, *args, **kwargs):
        if "transactional-update.conf" in str(path):
            if transactional:
                return MagicMock()
            raise FileNotFoundError(path)
        return product_file

    return _open


class TestParseSystem:
    @patch("mtui.hosts.target.parsers.system.product")
    def test_parse_suse_system(self, mock_product_module):
        """Test parsing a SUSE system with products.d."""
        conn, sftp = _mock_connection_with_sftp()

        # List products.d - return prod files
        sftp.listdir.return_value = ["SLES.prod", "sle-module-basesystem.prod"]

        # readlink for baseproduct
        sftp.readlink.return_value = "SLES.prod"

        # Mock the SFTP file open as context manager
        base_file = MagicMock()
        addon_file = MagicMock()
        # sftp.open sequence: base product, addon product, then the two
        # transactional-update.conf probes (usr-etc then etc), both missing.
        sftp.open.side_effect = [
            base_file,
            addon_file,
            FileNotFoundError("not found"),
            FileNotFoundError("not found"),
        ]

        mock_product_module.parse_product.side_effect = [
            ("SLES", "15-SP5", "x86_64"),
            ("sle-module-basesystem", "15-SP5", "x86_64"),
        ]

        from mtui.hosts.target.parsers.system import parse_system

        system, transactional = parse_system(conn)

        assert system.get_base().name == "SLES"
        assert transactional is False
        # The whole parse_system call ran inside a single SFTP session.
        assert conn.sftp_session.call_count == 1

    @patch("mtui.hosts.target.parsers.system.product")
    def test_parse_non_suse_system(self, mock_product_module):
        """Test parsing a non-SUSE system falls back to os-release."""
        conn, sftp = _mock_connection_with_sftp()

        # sftp.listdir raises OSError (no products.d)
        sftp.listdir.side_effect = OSError("not found")

        # os-release file
        mock_product_module.parse_os_release.return_value = (
            "ubuntu",
            "22.04",
            "x86_64",
        )

        os_release_file = MagicMock()
        sftp.open.return_value = os_release_file

        from mtui.hosts.target.parsers.system import parse_system

        system, transactional = parse_system(conn)

        assert system.get_base().name == "ubuntu"
        assert transactional is False
        assert conn.sftp_session.call_count == 1

    @patch("mtui.hosts.target.parsers.system.product")
    def test_parse_transactional_system(self, mock_product_module):
        """A SL-Micro host with transactional-update.conf is transactional.

        Mirrors a real SL-Micro 6.1 host: SL-Micro.prod base, one extras
        addon, and /usr/etc/transactional-update.conf present.
        """
        conn, sftp = _mock_connection_with_sftp()
        sftp.listdir.return_value = ["SL-Micro.prod", "SL-Micro-Extras.prod"]
        sftp.readlink.return_value = "SL-Micro.prod"
        sftp.open.side_effect = _dispatch_open(transactional=True)
        mock_product_module.parse_product.side_effect = [
            ("SL-Micro", "6.1", "x86_64"),
            ("SL-Micro-Extras", "6.1", "x86_64"),
        ]

        from mtui.hosts.target.parsers.system import parse_system

        system, transactional = parse_system(conn)

        assert system.get_base().name == "SL-Micro"
        assert system.get_base().version == "6.1"
        assert transactional is True

    @patch("mtui.hosts.target.parsers.system.product")
    def test_parse_transactional_system_with_etc_config(self, mock_product_module):
        """Older transactional layout: config in /etc, not /usr/etc.

        SLE Micro 5.x / MicroOS keep transactional-update.conf in /etc;
        the detector must still recognise such hosts as transactional.
        """
        conn, sftp = _mock_connection_with_sftp()
        sftp.listdir.return_value = ["SLE-Micro.prod"]
        sftp.readlink.return_value = "SLE-Micro.prod"
        product_file = MagicMock()

        def _open(path, *args, **kwargs):
            p = str(path)
            if p == "/usr/etc/transactional-update.conf":
                raise FileNotFoundError(p)  # newer location absent
            if p == "/etc/transactional-update.conf":
                return MagicMock()  # older location present
            return product_file

        sftp.open.side_effect = _open
        mock_product_module.parse_product.side_effect = [
            ("SLE-Micro", "5.5", "x86_64"),
        ]

        from mtui.hosts.target.parsers.system import parse_system

        _, transactional = parse_system(conn)

        assert transactional is True

    @patch("mtui.hosts.target.parsers.system.product")
    def test_parse_non_suse_without_os_release_falls_back_to_rhel(
        self, mock_product_module
    ):
        """A non-SUSE host with no /etc/os-release falls back to rhel 6."""
        conn, sftp = _mock_connection_with_sftp()
        sftp.listdir.side_effect = OSError("no products.d")
        sftp.open.side_effect = FileNotFoundError("no os-release")

        from mtui.hosts.target.parsers.system import parse_system

        system, transactional = parse_system(conn)

        assert system.get_base().name == "rhel"
        assert system.get_base().version == "6"
        assert transactional is False
        mock_product_module.parse_os_release.assert_not_called()

    @patch("mtui.hosts.target.parsers.system.product")
    def test_parse_sles_sap_12_adds_sles_and_ha_addons(self, mock_product_module):
        """SLES_SAP 12 implicitly carries SLES and sle-ha repos (workaround)."""
        conn, sftp = _mock_connection_with_sftp()
        sftp.listdir.return_value = ["SLES_SAP.prod"]
        sftp.readlink.return_value = "SLES_SAP.prod"
        sftp.open.side_effect = _dispatch_open(transactional=False)
        mock_product_module.parse_product.side_effect = [
            ("SLES_SAP", "12-SP5", "x86_64"),
        ]

        from mtui.hosts.target.parsers.system import parse_system

        system, _ = parse_system(conn)

        addons = {(p.name, p.version) for p in system.get_addons()}
        assert ("SLES", "12-SP5") in addons
        assert ("sle-ha", "12-SP5") in addons

    @patch("mtui.hosts.target.parsers.system.product")
    def test_parse_dangling_baseproduct_symlink(self, mock_product_module):
        """A dangling baseproduct symlink warns and degrades, not crash.

        ``readlink`` resolves to ``SLES.prod`` but opening it raises
        ``OSError`` (the target file is gone). parse_system must not crash:
        it flags ``dangling_base`` and derives a best-effort base name from
        the symlink target.
        """
        conn, sftp = _mock_connection_with_sftp()
        sftp.listdir.return_value = ["SLES.prod"]
        sftp.readlink.return_value = "SLES.prod"

        def _open(path, *args, **kwargs):
            p = str(path)
            if p == "/etc/products.d/SLES.prod":
                raise OSError("dangling symlink target missing")
            if "transactional-update.conf" in p:
                raise FileNotFoundError(p)
            return MagicMock()

        sftp.open.side_effect = _open

        from mtui.hosts.target.parsers.system import parse_system

        system, transactional = parse_system(conn)

        assert system.dangling_base is True
        # Best-effort base name from the symlink target ("SLES.prod").
        assert system.get_base().name == "SLES"
        assert system.get_base().version == ""
        assert transactional is False
        # The missing base file was never handed to parse_product.
        mock_product_module.parse_product.assert_not_called()

    @patch("mtui.hosts.target.parsers.system.product")
    def test_parse_absolute_baseproduct_symlink(self, mock_product_module):
        """An absolute baseproduct symlink target resolves, not false-dangling.

        Some hosts have ``/etc/products.d/baseproduct`` pointing at the
        absolute path ``/etc/products.d/SLES.prod`` rather than the bare
        ``SLES.prod``. The target file exists, so parse_system must read it
        normally (not concatenate it into ``/etc/products.d/<abspath>`` and
        misreport a dangling symlink).
        """
        conn, sftp = _mock_connection_with_sftp()
        sftp.listdir.return_value = ["SLES.prod"]
        sftp.readlink.return_value = "/etc/products.d/SLES.prod"

        def _open(path, *args, **kwargs):
            p = str(path)
            # Only the correctly-normalised path exists; a doubled path
            # (the pre-fix bug) raises OSError like a real dangling target.
            if p == "/etc/products.d/SLES.prod":
                return MagicMock()
            if "transactional-update.conf" in p:
                raise FileNotFoundError(p)
            raise OSError(f"no such file: {p}")

        sftp.open.side_effect = _open
        mock_product_module.parse_product.side_effect = [
            ("SLES", "15-SP6", "x86_64"),
        ]

        from mtui.hosts.target.parsers.system import parse_system

        system, transactional = parse_system(conn)

        assert system.dangling_base is False
        assert system.get_base().name == "SLES"
        assert system.get_base().version == "15-SP6"
        assert transactional is False

    @patch("mtui.hosts.target.parsers.system.product")
    def test_parse_missing_baseproduct_symlink(self, mock_product_module):
        """An absent/empty baseproduct symlink degrades instead of crashing."""
        conn, sftp = _mock_connection_with_sftp()
        sftp.listdir.return_value = []
        sftp.readlink.return_value = ""
        sftp.open.side_effect = _dispatch_open(transactional=False)

        from mtui.hosts.target.parsers.system import parse_system

        system, transactional = parse_system(conn)

        assert system.dangling_base is True
        assert system.get_base().name == "unknown"
        assert transactional is False

    @patch("mtui.hosts.target.parsers.system.product")
    def test_parse_sles_sap_16_adds_ha_addon(self, mock_product_module):
        """SLES_SAP 16 implicitly carries the sle-ha repo (workaround)."""
        conn, sftp = _mock_connection_with_sftp()
        sftp.listdir.return_value = ["SLES_SAP.prod"]
        sftp.readlink.return_value = "SLES_SAP.prod"
        sftp.open.side_effect = _dispatch_open(transactional=False)
        mock_product_module.parse_product.side_effect = [
            ("SLES_SAP", "16.0", "x86_64"),
        ]

        from mtui.hosts.target.parsers.system import parse_system

        system, _ = parse_system(conn)

        addons = {(p.name, p.version) for p in system.get_addons()}
        assert ("sle-ha", "16.0") in addons


class TestParseSystemAddonTolerance:
    @patch("mtui.hosts.target.parsers.system.product")
    def test_unreadable_addon_prod_is_skipped_not_fatal(
        self, mock_product_module, caplog
    ):
        """A dangling/unreadable addon .prod must not abort the connect.

        The base-product path already tolerates a dangling symlink; the
        addon loop did not: sftp.open raised OSError, which propagated
        out of parse_system into Target.connect and made the host
        impossible to add at all.
        """
        conn, sftp = _mock_connection_with_sftp()
        sftp.listdir.return_value = [
            "SLES.prod",
            "broken-addon.prod",
            "sle-module-basesystem.prod",
        ]
        sftp.readlink.return_value = "SLES.prod"

        base_file = MagicMock()
        addon_file = MagicMock()

        def _open(path, *args, **kwargs):
            p = str(path)
            if "transactional-update.conf" in p:
                raise FileNotFoundError(p)
            if "broken-addon" in p:
                raise OSError("dangling symlink")
            if "SLES.prod" in p:
                return base_file
            return addon_file

        sftp.open.side_effect = _open
        mock_product_module.parse_product.side_effect = [
            ("SLES", "15-SP5", "x86_64"),
            ("sle-module-basesystem", "15-SP5", "x86_64"),
        ]

        from mtui.hosts.target.parsers.system import parse_system

        with caplog.at_level("WARNING", logger="mtui.targer.parsers.system"):
            system, transactional = parse_system(conn)

        assert system.get_base().name == "SLES"
        assert transactional is False
        # The readable addon survived; the broken one was skipped, warned.
        addon_names = {a.name for a in system.get_addons()}
        assert "sle-module-basesystem" in addon_names
        assert any(
            "skipping unreadable addon product file" in r.message
            for r in caplog.records
        )
