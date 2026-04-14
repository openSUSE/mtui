"""Tests for the mtui target parsers modules."""

import io
from unittest.mock import MagicMock, patch

import pytest

from mtui.target.parsers.product import parse_os_release, parse_product
from mtui.types import Product


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


class TestParseSystem:
    @patch("mtui.target.parsers.system.product")
    def test_parse_suse_system(self, mock_product_module):
        """Test parsing a SUSE system with products.d."""
        conn = MagicMock()
        conn.hostname = "host1"

        # List products.d - return prod files
        conn.sftp_listdir.return_value = ["SLES.prod", "sle-module-basesystem.prod"]

        # readlink for baseproduct
        conn.sftp_readlink.return_value = "SLES.prod"

        # Mock the SFTP file open as context manager
        base_file = MagicMock()
        addon_file = MagicMock()
        transactional_check = MagicMock()

        # sftp_open calls sequence:
        # 1. base product file
        # 2. addon product file
        # 3. transactional-update.conf (FileNotFoundError)
        conn.sftp_open.side_effect = [
            base_file,
            addon_file,
            FileNotFoundError("not found"),
        ]

        mock_product_module.parse_product.side_effect = [
            ("SLES", "15-SP5", "x86_64"),
            ("sle-module-basesystem", "15-SP5", "x86_64"),
        ]

        from mtui.target.parsers.system import parse_system

        system, transactional = parse_system(conn)

        assert system.get_base().name == "SLES"
        assert transactional is False

    @patch("mtui.target.parsers.system.product")
    def test_parse_non_suse_system(self, mock_product_module):
        """Test parsing a non-SUSE system falls back to os-release."""
        conn = MagicMock()
        conn.hostname = "host1"

        # sftp_listdir raises OSError (no products.d)
        conn.sftp_listdir.side_effect = OSError("not found")

        # os-release file
        mock_product_module.parse_os_release.return_value = (
            "ubuntu",
            "22.04",
            "x86_64",
        )

        os_release_file = MagicMock()
        conn.sftp_open.return_value = os_release_file

        from mtui.target.parsers.system import parse_system

        system, transactional = parse_system(conn)

        assert system.get_base().name == "ubuntu"
        assert transactional is False
