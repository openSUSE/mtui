"""Mutation-killing pins for the ``zypper`` post-action checks.

A full mutmut run left survivors in the four ``mtui.update_workflow.checks``
modules because the existing tests only assert ``pytest.raises(UpdateError)``
(never the ``reason``/``host`` carried by the exception) and only assert
substrings of the printed diagnostic blocks (never the exact slice bounds).

These tests pin, per module:

* the exact ``UpdateError(reason, host)`` arguments of every raise site,
* the exact printed slices of the "Additional rpm output" and
  "not supported by its vendor" blocks in ``checks.update`` (including the
  ``yellow('warning')`` highlight and the slice start/end computation), and
* the compound conditions on the exitcode-104/106 branches of
  ``checks.update`` (no raise / no warning when only one conjunct holds).
"""

from __future__ import annotations

import logging

import pytest

import mtui.cli.colors.mode as color_mode
from mtui.support.exceptions import UpdateError
from mtui.update_workflow.checks.downgrade import zypper as downgrade_zypper
from mtui.update_workflow.checks.install import zypper as install_zypper
from mtui.update_workflow.checks.prepare import zypper as prepare_zypper
from mtui.update_workflow.checks.update import zypper as update_zypper

#: ``yellow("warning")`` with colours forced on (see ``mtui.cli.colors.ansi``).
YELLOW_WARNING = "\x1b[1;33mwarning\x1b[1;m\x1b[0m"


# ---------------------------------------------------------------------------
# checks.update -- raise sites carry (reason, host)
# ---------------------------------------------------------------------------


def test_update_zypper_stdin_and_exitcode_104_reason_and_host() -> None:
    """``zypper`` in stdin + exitcode 104 raises ("package not found", host)."""
    with pytest.raises(UpdateError) as ei:
        update_zypper("h", "", "zypper in pkg", "", 104)
    assert ei.value.reason == "package not found"
    assert ei.value.host == "h"


def test_update_zypper_exitcode_104_without_zypper_stdin_passes() -> None:
    """Exitcode 104 alone (no ``zypper`` in stdin) does not raise."""
    assert update_zypper("h", "", "echo hi", "", 104) is None


def test_update_zypper_zypp_transaction_lock_reason_and_host() -> None:
    """A ZYpp transaction lock raises ("update stack locked", host)."""
    with pytest.raises(UpdateError) as ei:
        update_zypper("h", "", "echo", "A ZYpp transaction is already in progress.", 0)
    assert ei.value.reason == "update stack locked"
    assert ei.value.host == "h"


def test_update_zypper_system_management_locked_reason_and_host() -> None:
    """A "System management is locked" stderr raises ("update stack locked", host)."""
    with pytest.raises(UpdateError) as ei:
        update_zypper("h", "", "echo", "System management is locked", 0)
    assert ei.value.reason == "update stack locked"
    assert ei.value.host == "h"


def test_update_zypper_unresolved_dep_reason_and_host() -> None:
    """``(c): c`` in stdout raises ("Dependency Error", host)."""
    with pytest.raises(UpdateError) as ei:
        update_zypper("h", "(c): c", "echo", "", 0)
    assert ei.value.reason == "Dependency Error"
    assert ei.value.host == "h"


def test_update_zypper_rpm_error_reason_and_host() -> None:
    """``Error:`` in stderr raises ("RPM Error", host)."""
    with pytest.raises(UpdateError) as ei:
        update_zypper("h", "", "echo", "Error: bad", 0)
    assert ei.value.reason == "RPM Error"
    assert ei.value.host == "h"


# ---------------------------------------------------------------------------
# checks.update -- exitcode-106 warning fires only when BOTH conjuncts hold
# ---------------------------------------------------------------------------


def test_update_zypper_stdin_and_exitcode_106_warns(caplog) -> None:
    """``zypper`` in stdin + exitcode 106 logs the 106 warning, no raise."""
    with caplog.at_level(logging.WARNING, logger="mtui.checks.update"):
        assert update_zypper("h", "", "zypper in pkg", "", 106) is None
    assert any("exitcode 106" in r.message for r in caplog.records)


def test_update_zypper_stdin_zypper_exitcode_0_no_106_warning(caplog) -> None:
    """``zypper`` in stdin with exitcode 0 must NOT log the 106 warning."""
    with caplog.at_level(logging.WARNING, logger="mtui.checks.update"):
        assert update_zypper("h", "", "zypper in pkg", "", 0) is None
    assert not any("exitcode 106" in r.message for r in caplog.records)


def test_update_zypper_exitcode_106_without_zypper_stdin_no_warning(caplog) -> None:
    """Exitcode 106 without ``zypper`` in stdin must NOT log the 106 warning."""
    with caplog.at_level(logging.WARNING, logger="mtui.checks.update"):
        assert update_zypper("h", "", "echo hi", "", 106) is None
    assert not any("exitcode 106" in r.message for r in caplog.records)


# ---------------------------------------------------------------------------
# checks.update -- exact "Additional rpm output" print slice
# ---------------------------------------------------------------------------

#: Payload with decoys around the block: a "Retrieving" line and a "warning"
#: line BEFORE the marker, a second marker and a second "Retrieving" line
#: AFTER the block, so any mutation of the slice bounds (``find`` vs
#: ``rfind``, dropped ``+ len(marker)``, dropped ``start`` argument, mutated
#: marker string) selects a visibly different slice.
RPM_OUTPUT_STDOUT = (
    "Retrieving package early\n"
    "warning: early warning\n"
    "Additional rpm output:\n"
    "warning: /etc/foo saved as /etc/foo.rpmsave\n"
    "more output\n"
    "Retrieving package bar\n"
    "Additional rpm output:\n"
    "warning: second block\n"
    "Retrieving package baz\n"
)


def test_update_zypper_rpm_output_exact_slice_with_highlight(
    capsys, monkeypatch
) -> None:
    """The first rpm-output block is printed exactly, "warning" highlighted.

    The slice starts right after the FIRST ``Additional rpm output:`` marker
    and ends at the next ``Retrieving``; every ``warning`` inside it is
    wrapped in the yellow ANSI escape.
    """
    monkeypatch.setattr(color_mode, "_mode", "always")
    assert update_zypper("h", RPM_OUTPUT_STDOUT, "echo hi", "", 0) is None
    expected = (
        f"\n{YELLOW_WARNING}: /etc/foo saved as /etc/foo.rpmsave\nmore output\n\n"
    )
    assert capsys.readouterr().out == expected


def test_update_zypper_rpm_output_exact_slice_colors_off(capsys, monkeypatch) -> None:
    """With colours disabled the block is printed verbatim (no escapes)."""
    monkeypatch.setattr(color_mode, "_mode", "never")
    assert update_zypper("h", RPM_OUTPUT_STDOUT, "echo hi", "", 0) is None
    expected = "\nwarning: /etc/foo saved as /etc/foo.rpmsave\nmore output\n\n"
    assert capsys.readouterr().out == expected


def test_update_zypper_no_rpm_output_marker_prints_nothing(capsys) -> None:
    """Without the marker nothing is printed at all."""
    assert update_zypper("h", "Installing: foo\ndone\n", "echo hi", "", 0) is None
    assert capsys.readouterr().out == ""


# ---------------------------------------------------------------------------
# checks.update -- exact "not supported by its vendor" print slice
# ---------------------------------------------------------------------------

#: Payload with a blank line ("\n\n") BEFORE the marker, a second vendor
#: block and a second trailing blank line AFTER it, so ``find`` vs ``rfind``
#: and a dropped ``start`` argument each select a different slice.
VENDOR_STDOUT = (
    "Loading repository data...\n"
    "\n"
    "The following package is not supported by its vendor:\n"
    "  foo-1.2.3-1.1.x86_64\n"
    "\n"
    "The following package is not supported by its vendor:\n"
    "  bar-2.0-1.1.x86_64\n"
    "\n"
    "Continue? [y/n]\n"
    "\n"
    "tail\n"
)


def test_update_zypper_vendor_block_exact_slice(capsys) -> None:
    """The FIRST vendor block is printed exactly: marker line up to ``\\n\\n``."""
    assert update_zypper("h", VENDOR_STDOUT, "echo hi", "", 0) is None
    expected = (
        "The following package is not supported by its vendor:\n"
        "  foo-1.2.3-1.1.x86_64\n"
    )
    assert capsys.readouterr().out == expected


# ---------------------------------------------------------------------------
# checks.install -- every path's exitcode classification and (reason, host)
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("exitcode", [0, 100, 101, 102, 103, 106])
def test_install_zypper_success_exitcodes_pass(exitcode: int) -> None:
    """Success exitcodes return ``None`` even with scary output around.

    The stdout/stderr payloads deliberately carry the RPM-error marker:
    the success-exitcode early return must take precedence over every
    later marker check.
    """
    assert (
        install_zypper("h", "(c): c", "in pkg", "Error: scriptlet noise", exitcode)
        is None
    )


@pytest.mark.parametrize("exitcode", [104, 4, 5, 8])
def test_install_zypper_package_not_found_exitcodes(exitcode: int) -> None:
    """Exitcodes 104/4/5/8 raise ("package not found", host)."""
    with pytest.raises(UpdateError) as ei:
        install_zypper("h", "", "in pkg", "", exitcode)
    assert ei.value.reason == "package not found"
    assert ei.value.host == "h"


@pytest.mark.parametrize(
    "stderr",
    [
        "A ZYpp transaction is already in progress.",
        "System management is locked",
    ],
)
def test_install_zypper_lock_markers_reason_and_host(stderr: str) -> None:
    """Either lock marker alone in stderr raises ("update stack locked", host)."""
    with pytest.raises(UpdateError) as ei:
        install_zypper("h", "", "in pkg", stderr, 1)
    assert ei.value.reason == "update stack locked"
    assert ei.value.host == "h"


def test_install_zypper_rpm_error_reason_and_host() -> None:
    """``Error:`` in stderr raises ("RPM Error", host)."""
    with pytest.raises(UpdateError) as ei:
        install_zypper("h", "", "in pkg", "Error: something bad", 1)
    assert ei.value.reason == "RPM Error"
    assert ei.value.host == "h"


def test_install_zypper_dependency_error_reason_and_host() -> None:
    """``(c): c`` in stdout raises ("Dependency Error", host)."""
    with pytest.raises(UpdateError) as ei:
        install_zypper("h", "(c): c", "in pkg", "", 1)
    assert ei.value.reason == "Dependency Error"
    assert ei.value.host == "h"


@pytest.mark.parametrize("exitcode", [1, 9, 99])
def test_install_zypper_unknown_error_reason_and_host(exitcode: int) -> None:
    """Unclassified failures raise exactly ("Unknown Error", host).

    Exitcode 9 in particular must NOT be classified as "package not found"
    (it is not a member of the (104, 4, 5, 8) tuple).
    """
    with pytest.raises(UpdateError) as ei:
        install_zypper("h", "", "in pkg", "", exitcode)
    assert ei.value.reason == "Unknown Error"
    assert ei.value.host == "h"


# ---------------------------------------------------------------------------
# checks.prepare -- (reason, host) on every raise site
# ---------------------------------------------------------------------------


def test_prepare_zypper_zypp_lock_reason_and_host() -> None:
    """A ZYpp transaction lock raises ("update stack locked", host)."""
    with pytest.raises(UpdateError) as ei:
        prepare_zypper(
            "h", "", "in pkg", "A ZYpp transaction is already in progress.", 0
        )
    assert ei.value.reason == "update stack locked"
    assert ei.value.host == "h"


def test_prepare_zypper_system_management_locked_reason_and_host() -> None:
    """A "System management is locked" stderr raises ("update stack locked", host)."""
    with pytest.raises(UpdateError) as ei:
        prepare_zypper("h", "", "in pkg", "System management is locked", 0)
    assert ei.value.reason == "update stack locked"
    assert ei.value.host == "h"


def test_prepare_zypper_dependency_error_reason_and_host() -> None:
    """``(c): c`` in stdout raises ("Dependency Error", host)."""
    with pytest.raises(UpdateError) as ei:
        prepare_zypper("h", "(c): c", "in pkg", "", 0)
    assert ei.value.reason == "Dependency Error"
    assert ei.value.host == "h"


def test_prepare_zypper_rpm_error_reason_and_host() -> None:
    """``Error:`` in stderr raises ("RPM Error", host)."""
    with pytest.raises(UpdateError) as ei:
        prepare_zypper("h", "", "in pkg", "Error: foo", 0)
    assert ei.value.reason == "RPM Error"
    assert ei.value.host == "h"


# ---------------------------------------------------------------------------
# checks.downgrade -- (reason, host) on every raise site
# ---------------------------------------------------------------------------


def test_downgrade_zypper_zypp_lock_reason_and_host() -> None:
    """A ZYpp transaction lock raises ("update stack locked", host)."""
    with pytest.raises(UpdateError) as ei:
        downgrade_zypper(
            "h", "", "in pkg", "A ZYpp transaction is already in progress.", 0
        )
    assert ei.value.reason == "update stack locked"
    assert ei.value.host == "h"


def test_downgrade_zypper_system_management_locked_reason_and_host() -> None:
    """A "System management is locked" stderr raises ("update stack locked", host)."""
    with pytest.raises(UpdateError) as ei:
        downgrade_zypper("h", "", "in pkg", "System management is locked", 0)
    assert ei.value.reason == "update stack locked"
    assert ei.value.host == "h"


def test_downgrade_zypper_dependency_error_reason_and_host() -> None:
    """``(c): c`` in stdout raises ("Dependency Error", host)."""
    with pytest.raises(UpdateError) as ei:
        downgrade_zypper("h", "(c): c", "in pkg", "", 0)
    assert ei.value.reason == "Dependency Error"
    assert ei.value.host == "h"


def test_downgrade_zypper_exitcode_104_reason_and_host() -> None:
    """Exitcode 104 raises ("Unspecified Error", host)."""
    with pytest.raises(UpdateError) as ei:
        downgrade_zypper("h", "", "in pkg", "", 104)
    assert ei.value.reason == "Unspecified Error"
    assert ei.value.host == "h"
