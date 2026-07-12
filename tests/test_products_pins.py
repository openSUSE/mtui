"""Mutation-killing pins for ``mtui.test_reports.products`` normalizers.

A mutmut run left survivors in the product normalizers because the existing
suite asserts only the normalized product *name* (``out[0][0]``).  Mutants
corrupting the rewritten version field (e.g. dropping the ``-LTSS`` strip),
the arch field, or the argument passed by the top-level ``normalize()``
dispatcher all survived.

These tests pin the complete ``[name, version, arch]`` triple for every
branch and drive the dispatcher with real payloads (no mocks) so that
argument-dropping and needle-corrupting mutants die.  Product versions key
``update_repos`` (see ``metadata_parsers.obsrepoparse``), so a silently
mangled version breaks repository mapping.
"""

from __future__ import annotations

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

# Inputs are given as immutable tuples and copied to fresh lists inside each
# test: the normalizers mutate their argument in place and are not all
# idempotent, so shared parametrize values must never be mutated.


@pytest.mark.parametrize(
    ("fn", "before", "expected"),
    [
        # --- sle11: full triple per branch ---
        (
            normalize_sle11,
            ("SLE-SDK", "11-SP4", "x86_64"),
            ("sle-sdk", "11-SP4", "x86_64"),
        ),
        (
            normalize_sle11,
            ("SLE-SAP-AIO", "11-SP1", "x86_64"),
            ("SUSE_SLES_SAP", "11-SP1", "x86_64"),
        ),
        (
            normalize_sle11,
            ("SLE-SERVER", "11-SP4-LTSS", "s390x"),
            ("SUSE_SLES", "11-SP4", "s390x"),
        ),
        (
            normalize_sle11,
            ("SLE-SERVER", "11-SP3-CLIENT-TOOLS", "x86_64"),
            ("SUSE_SLES", "11-SP3", "x86_64"),
        ),
        (
            normalize_sle11,
            ("SLE-SERVER", "11-SP4", "i586"),
            ("SUSE_SLES", "11-SP4", "i586"),
        ),
        # The CORE branch rewrites and then falls through to the final return.
        (
            normalize_sle11,
            ("SLE-SERVER", "11-SP3-LTSS-EXTREME-CORE", "x86_64"),
            ("SUSE_SLES_LTSS-EXTREME-CORE", "11-SP3", "x86_64"),
        ),
        # Suffix guard: TERADATA/SECURITY/PUBCLOUD versions must NOT take the
        # SUSE_SLES branch even for SLE-SERVER.
        (
            normalize_sle11,
            ("SLE-SERVER", "11-SP1-TERADATA", "x86_64"),
            ("teradata", "11-SP1", "x86_64"),
        ),
        (
            normalize_sle11,
            ("SLE-SERVER", "11-SP4-SECURITY", "x86_64"),
            ("security", "11", "x86_64"),
        ),
        (
            normalize_sle11,
            ("SLE-SERVER", "11-SP4-PUBCLOUD", "x86_64"),
            ("sle-module-pubcloud", "11", "x86_64"),
        ),
        (
            normalize_sle11,
            ("SLE-SMT", "11-SP3", "x86_64"),
            ("sle-smt", "11-SP3", "x86_64"),
        ),
        (
            normalize_sle11,
            ("SLE-HAE", "11-SP4", "x86_64"),
            ("sle-hae", "11-SP4", "x86_64"),
        ),
        (
            normalize_sle11,
            ("SLE-Unknown", "11-SP4", "x86_64"),
            ("SLE-Unknown", "11-SP4", "x86_64"),
        ),
        # --- sle12: full triple per branch ---
        (
            normalize_sle12,
            ("SLE-SERVER", "12-SP5-LTSS-Extended-Security", "x86_64"),
            ("SLES-LTSS-Extended-Security", "12-SP5", "x86_64"),
        ),
        (
            normalize_sle12,
            ("SLE-SERVER", "12-SP5-LTSS-ERICSSON", "s390x"),
            ("SLES-LTSS-ERICSSON", "12-SP5", "s390x"),
        ),
        (
            normalize_sle12,
            ("SLE-SERVER", "12-SP3-LTSS-SAP", "x86_64"),
            ("SLES-LTSS-SAP", "12-SP3", "x86_64"),
        ),
        (
            normalize_sle12,
            ("SLE-SERVER", "12-SP1-LTSS-TERADATA", "x86_64"),
            ("SLES_LTSS_TERADATA", "12-SP1", "x86_64"),
        ),
        (
            normalize_sle12,
            ("SLE-SERVER", "12-SP3-LTSS", "ppc64le"),
            ("SLES-LTSS", "12-SP3", "ppc64le"),
        ),
        (
            normalize_sle12,
            ("SLE-SERVER", "12-SP1-TERADATA", "x86_64"),
            ("SLES_TERADATA", "12-SP1", "x86_64"),
        ),
        (
            normalize_sle12,
            ("SLE-SERVER", "12-SP5", "x86_64"),
            ("SLES", "12-SP5", "x86_64"),
        ),
        (
            normalize_sle12,
            ("SLE-DESKTOP", "12-SP5", "x86_64"),
            ("SLED", "12-SP5", "x86_64"),
        ),
        (
            normalize_sle12,
            ("SLE-RPI", "12-SP2", "aarch64"),
            ("SLES_RPI", "12-SP2", "aarch64"),
        ),
        (
            normalize_sle12,
            ("SLE-SAP", "12-SP5", "x86_64"),
            ("SLES_SAP", "12-SP5", "x86_64"),
        ),
        (
            normalize_sle12,
            ("SLE-Module-Web-Scripting", "12", "x86_64"),
            ("sle-module-web-scripting", "12", "x86_64"),
        ),
        # --- sle15: full triple per branch ---
        (
            normalize_sle15,
            ("SLE-Product-SLES", "15-SP1-LTSS-TERADATA", "x86_64"),
            ("SLES-LTSS-TERADATA", "15-SP1", "x86_64"),
        ),
        (
            normalize_sle15,
            ("SLE-Product-SLES", "15-SP2-LTSS", "s390x"),
            ("SLES-LTSS", "15-SP2", "s390x"),
        ),
        (
            normalize_sle15,
            ("SLE-Product-SLES", "15-SP4-ERICSSON", "x86_64"),
            ("ERICSSON", "15-SP4", "x86_64"),
        ),
        (
            normalize_sle15,
            ("SLE-Product-SLES", "15-SP1-TERADATA", "x86_64"),
            ("SLES_TERADATA", "15-SP1", "x86_64"),
        ),
        (
            normalize_sle15,
            ("SLE-Product-SLES", "15-SP6", "x86_64"),
            ("SLES", "15-SP6", "x86_64"),
        ),
        (
            normalize_sle15,
            ("SLE-Product-SLED", "15-SP6", "x86_64"),
            ("SLED", "15-SP6", "x86_64"),
        ),
        (
            normalize_sle15,
            ("SLE-Product-WE", "15-SP6", "x86_64"),
            ("sle-we", "15-SP6", "x86_64"),
        ),
        (
            normalize_sle15,
            ("SLE-Product-HA", "15-SP5", "ppc64le"),
            ("sle-ha", "15-SP5", "ppc64le"),
        ),
        (
            normalize_sle15,
            ("SLE-Product-HPC", "15-SP5", "x86_64"),
            ("SLE_HPC", "15-SP5", "x86_64"),
        ),
        (
            normalize_sle15,
            ("SLE-Product-SLES_SAP", "15-SP4", "x86_64"),
            ("SLES_SAP", "15-SP4", "x86_64"),
        ),
        (
            normalize_sle15,
            ("SLE-Product-RT", "15-SP5", "x86_64"),
            ("SLE_RT", "15-SP5", "x86_64"),
        ),
        (
            normalize_sle15,
            ("SLE-Module-Basesystem", "15-SP6", "x86_64"),
            ("sle-module-basesystem", "15-SP6", "x86_64"),
        ),
        # --- misc: full triple per function ---
        (
            normalize_ses,
            ("Storage", "7.1", "x86_64"),
            ("ses", "7.1", "x86_64"),
        ),
        (
            normalize_rt,
            ("SLE-RT", "12-SP5", "x86_64"),
            ("SUSE-Linux-Enterprise-RT", "12-SP5", "x86_64"),
        ),
        (
            normalize_manager,
            ("SLE-Manager-Tools", "12", "x86_64"),
            ("sle-manager-tools", "12", "x86_64"),
        ),
        (
            normalize_manager,
            ("SUSE-Manager-Server", "4.3", "x86_64"),
            ("SUSE-Manager-Server", "4.3", "x86_64"),
        ),
        # normalize_osle swaps version <- arch and hardcodes arch to x86_64.
        (
            normalize_osle,
            ("openSUSE-SLE", "15.4", "openSUSE-Leap-15.4"),
            ("leap", "openSUSE-Leap-15.4", "x86_64"),
        ),
    ],
)
def test_normalize_full_triple(fn, before, expected) -> None:
    payload = [list(before)]
    out = fn(payload)
    assert out is payload  # normalizers mutate and return the same object
    assert out == [list(expected)]


# ---------------------------------------------------------------------------
# Top-level normalize() dispatcher with real payloads (no mocks): kills
# mutants that pass None to the family normalizers or corrupt the match
# strings, which mock-based dispatch tests cannot see.
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("before", "expected"),
    [
        # SLE-RT is dispatched by name before any version comparison.
        (
            ("SLE-RT", "12-SP5", "x86_64"),
            ("SUSE-Linux-Enterprise-RT", "12-SP5", "x86_64"),
        ),
        (("SLE-SERVER", "11-SP4-LTSS", "i586"), ("SUSE_SLES", "11-SP4", "i586")),
        (
            ("SLE-SERVER", "12-SP5-LTSS-ERICSSON", "s390x"),
            ("SLES-LTSS-ERICSSON", "12-SP5", "s390x"),
        ),
        (
            ("SLE-Product-SLES", "15-SP1-LTSS-TERADATA", "x86_64"),
            ("SLES-LTSS-TERADATA", "15-SP1", "x86_64"),
        ),
        (("Storage", "7.1", "x86_64"), ("ses", "7.1", "x86_64")),
        (
            ("SUSE-Manager-Server", "4.3", "x86_64"),
            ("SUSE-Manager-Server", "4.3", "x86_64"),
        ),
        # Version must not start with 11/12/15 to reach the manager branch.
        (
            ("SLE-Manager-Tools", "16", "x86_64"),
            ("sle-manager-tools", "16", "x86_64"),
        ),
        # Leap: obsrepoparse yields e.g. ["Updates", "openSUSE-SLE", "15.4"]
        # (project.split(":")[-3:]), so the match is on the middle field.
        (("Updates", "openSUSE-SLE", "15.4"), ("leap", "15.4", "x86_64")),
        # Cornercase passthrough stays untouched.
        (("NoMatch", "13.2", "aarch64"), ("NoMatch", "13.2", "aarch64")),
    ],
)
def test_normalize_dispatch_full_triple(before, expected) -> None:
    payload = [list(before)]
    out = normalize(payload)
    assert out is payload
    assert out == [list(expected)]


def test_normalize_preserves_repo_name_companion() -> None:
    # obsrepoparse feeds (project-triple, repo-name) tuples through
    # normalize(); only the triple may be rewritten.
    repo = "SUSE_Updates_SLE-SERVER_12-SP5-LTSS_x86_64"
    payload = (["SLE-SERVER", "12-SP5-LTSS", "x86_64"], repo)
    out = normalize(payload)
    assert out is payload
    assert out[0] == ["SLES-LTSS", "12-SP5", "x86_64"]
    assert out[1] == repo


# ---------------------------------------------------------------------------
# normalize_16: the rename must preserve version and arch.
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("before", "expected"),
    [
        (
            Product("SLES-SAP", "16.0", "x86_64"),
            Product("SLES_SAP", "16.0", "x86_64"),
        ),
        (
            Product("SLES-HA", "16.0", "aarch64"),
            Product("sle-ha", "16.0", "aarch64"),
        ),
    ],
)
def test_normalize_16_full_product(before, expected) -> None:
    assert normalize_16(before) == expected


def test_normalize_16_passthrough_is_same_object() -> None:
    p = Product("SLES", "16.0", "x86_64")
    assert normalize_16(p) is p
