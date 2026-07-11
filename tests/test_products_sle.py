"""Tests for ``mtui.test_reports.products.*`` normalize functions."""

from __future__ import annotations

from copy import deepcopy
from unittest.mock import patch

import pytest

from mtui.test_reports.products import normalize, normalize_16
from mtui.test_reports.products.misc import (
    normalize_manager,
    normalize_osle,
    normalize_rt,
    normalize_ses,
)
from mtui.test_reports.products.sle11 import normalize_sle11
from mtui.test_reports.products.sle12 import normalize_sle12
from mtui.test_reports.products.sle15 import normalize_sle15
from mtui.types import Product


@pytest.mark.parametrize(
    ("fn", "before", "expected_first"),
    [
        # --- sle11 branches ---
        (normalize_sle11, [["SLE-SDK", "11", "x86_64"]], "sle-sdk"),
        (normalize_sle11, [["SLE-SAP-AIO", "11", "x86_64"]], "SUSE_SLES_SAP"),
        (normalize_sle11, [["SLE-SERVER", "11-LTSS", "x86_64"]], "SUSE_SLES"),
        (normalize_sle11, [["SLE-SMT", "11", "x86_64"]], "sle-smt"),
        (normalize_sle11, [["SLE-HAE", "11", "x86_64"]], "sle-hae"),
        (
            normalize_sle11,
            [["SLES-SAP", "11-CORE", "x86_64"]],
            "SUSE_SLES_LTSS-EXTREME-CORE",
        ),
        (normalize_sle11, [["SLES-SAP", "11-TERADATA", "x86_64"]], "teradata"),
        (normalize_sle11, [["SLES-SAP", "11-SECURITY", "x86_64"]], "security"),
        (
            normalize_sle11,
            [["SLES-SAP", "11-PUBCLOUD", "x86_64"]],
            "sle-module-pubcloud",
        ),
        # --- sle12 branches ---
        (
            normalize_sle12,
            [["SLE-SERVER", "12-LTSS-Extended-Security", "x86_64"]],
            "SLES-LTSS-Extended-Security",
        ),
        (
            normalize_sle12,
            [["SLE-SERVER", "12-LTSS-ERICSSON", "x86_64"]],
            "SLES-LTSS-ERICSSON",
        ),
        (
            normalize_sle12,
            [["SLE-SERVER", "12-LTSS-SAP", "x86_64"]],
            "SLES-LTSS-SAP",
        ),
        (
            normalize_sle12,
            [["SLE-SERVER", "12-LTSS-TERADATA", "x86_64"]],
            "SLES_LTSS_TERADATA",
        ),
        (normalize_sle12, [["SLE-SERVER", "12-LTSS", "x86_64"]], "SLES-LTSS"),
        (normalize_sle12, [["SLE-SERVER", "12-TERADATA", "x86_64"]], "SLES_TERADATA"),
        (normalize_sle12, [["SLE-SERVER", "12", "x86_64"]], "SLES"),
        (normalize_sle12, [["SLE-DESKTOP", "12", "x86_64"]], "SLED"),
        (normalize_sle12, [["SLE-RPI", "12", "x86_64"]], "SLES_RPI"),
        (normalize_sle12, [["SLE-SAP", "12", "x86_64"]], "SLES_SAP"),
        (normalize_sle12, [["SLE-Module-Web", "12", "x86_64"]], "sle-module-web"),
        # --- sle15 branches ---
        (
            normalize_sle15,
            [["SLE-Product-SLES", "15-LTSS-TERADATA", "x86_64"]],
            "SLES-LTSS-TERADATA",
        ),
        (
            normalize_sle15,
            [["SLE-Product-SLES", "15-LTSS", "x86_64"]],
            "SLES-LTSS",
        ),
        (
            normalize_sle15,
            [["SLE-Product-SLES", "15-ERICSSON", "x86_64"]],
            "ERICSSON",
        ),
        (
            normalize_sle15,
            [["SLE-Product-SLES", "15-TERADATA", "x86_64"]],
            "SLES_TERADATA",
        ),
        (normalize_sle15, [["SLE-Product-SLES", "15", "x86_64"]], "SLES"),
        (normalize_sle15, [["SLE-Product-SLED", "15", "x86_64"]], "SLED"),
        (normalize_sle15, [["SLE-Product-WE", "15", "x86_64"]], "sle-we"),
        (normalize_sle15, [["SLE-Product-HA", "15", "x86_64"]], "sle-ha"),
        (normalize_sle15, [["SLE-Product-HPC", "15", "x86_64"]], "SLE_HPC"),
        (
            normalize_sle15,
            [["SLE-Product-SLES_SAP", "15", "x86_64"]],
            "SLES_SAP",
        ),
        (normalize_sle15, [["SLE-Product-RT", "15", "x86_64"]], "SLE_RT"),
        (normalize_sle15, [["SLE-Module-Web", "15", "x86_64"]], "sle-module-web"),
        # --- misc ---
        (normalize_ses, [["Storage", "7", "x86_64"]], "ses"),
        (normalize_rt, [["SLE-RT", "12", "x86_64"]], "SUSE-Linux-Enterprise-RT"),
        (
            normalize_manager,
            [["SLE-Manager-Tools", "12", "x86_64"]],
            "sle-manager-tools",
        ),
        (
            normalize_manager,
            [["SUSE-Manager-Server", "4.1", "x86_64"]],
            "SUSE-Manager-Server",
        ),
        (
            normalize_osle,
            [["openSUSE-SLE", "15.4", "openSUSE-Leap-15.4"]],
            "leap",
        ),
    ],
)
def test_normalize_first_element(fn, before, expected_first) -> None:
    # The normalize functions mutate their argument in place, and the
    # parametrize table above is built once per module import. Feed each
    # call a copy so the table survives repeated in-process pytest runs
    # (mutmut re-enters pytest.main in one interpreter).
    out = fn(deepcopy(before))
    assert out[0][0] == expected_first


# ---------------------------------------------------------------------------
# normalize_16
# ---------------------------------------------------------------------------


def test_normalize_16_sles_sap_rewrites_name() -> None:
    p = Product("SLES-SAP", "16", "x86_64")
    out = normalize_16(p)
    assert out.name == "SLES_SAP"


def test_normalize_16_sles_ha_rewrites_name() -> None:
    p = Product("SLES-HA", "16", "x86_64")
    out = normalize_16(p)
    assert out.name == "sle-ha"


def test_normalize_16_passthrough_unchanged() -> None:
    p = Product("FooBar", "16", "x86_64")
    out = normalize_16(p)
    assert out is p


# ---------------------------------------------------------------------------
# normalize dispatcher
# ---------------------------------------------------------------------------


def test_normalize_dispatch_sle_rt() -> None:
    with patch("mtui.test_reports.products.normalize_rt", return_value="rt-out") as m:
        out = normalize([["SLE-RT", "12", "x86_64"]])
    m.assert_called_once()
    assert out == "rt-out"


def test_normalize_dispatch_sle11_prefix() -> None:
    with patch("mtui.test_reports.products.normalize_sle11", return_value="11") as m:
        out = normalize([["SLE-SDK", "11", "x86_64"]])
    m.assert_called_once()
    assert out == "11"


def test_normalize_dispatch_sle12_prefix() -> None:
    with patch("mtui.test_reports.products.normalize_sle12", return_value="12") as m:
        out = normalize([["SLE-SERVER", "12", "x86_64"]])
    m.assert_called_once()
    assert out == "12"


def test_normalize_dispatch_sle15_prefix() -> None:
    with patch("mtui.test_reports.products.normalize_sle15", return_value="15") as m:
        out = normalize([["SLE-Product-SLES", "15", "x86_64"]])
    m.assert_called_once()
    assert out == "15"


def test_normalize_dispatch_storage() -> None:
    with patch("mtui.test_reports.products.normalize_ses", return_value="ses-out") as m:
        out = normalize([["Storage", "7", "x86_64"]])
    m.assert_called_once()
    assert out == "ses-out"


def test_normalize_dispatch_manager() -> None:
    with patch("mtui.test_reports.products.normalize_manager", return_value="mgr") as m:
        out = normalize([["SUSE-Manager-Server", "4.1", "x86_64"]])
    m.assert_called_once()
    assert out == "mgr"


def test_normalize_dispatch_osle() -> None:
    # The dispatcher matches when the *version* field contains "openSUSE-SLE".
    with patch("mtui.test_reports.products.normalize_osle", return_value="leap") as m:
        out = normalize([["leap", "openSUSE-SLE-15.4", "x86_64"]])
    m.assert_called_once()
    assert out == "leap"


def test_normalize_dispatch_unmatched_returns_input() -> None:
    payload = [["NoMatch", "13", "x86_64"]]
    out = normalize(payload)
    assert out is payload
