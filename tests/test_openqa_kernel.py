"""Tests for ``mtui.data_sources.openqa.kernel``."""

from __future__ import annotations

from mtui.data_sources.openqa.kernel import KernelOpenQA
from mtui.types.test import Test as _Test  # aliased: avoids pytest "Test*" collection


def test_result_matrix_failed_annotates_result_and_lists_failed_modules() -> None:
    """A failed ltp_ test gets its ``result: failed`` annotated to ``failed:``.

    Regression: ``text.replace(...)`` discarded its result (strings are
    immutable), so the annotation never happened.
    """
    test = _Test(
        name="ltp_syscalls",
        result="failed",
        test_id=1,
        arch="x86_64",
        modules={"open01": "failed", "read01": "passed"},
    )
    [line] = KernelOpenQA._result_matrix([test])
    assert "result: failed:" in line
    # Only the failed module is listed.
    assert "open01:" in line
    assert "read01:" not in line


def test_result_matrix_passed_is_left_unannotated() -> None:
    """A passing ltp_ test keeps a plain ``result: passed`` line."""
    test = _Test(
        name="ltp_syscalls",
        result="passed",
        test_id=1,
        arch="x86_64",
        modules={},
    )
    [line] = KernelOpenQA._result_matrix([test])
    assert "result: passed" in line
    assert "failed:" not in line


def test_result_matrix_skips_non_ltp_tests() -> None:
    """Tests whose name does not start with ``ltp_`` produce no matrix rows."""
    test = _Test(
        name="boot",
        result="failed",
        test_id=1,
        arch="x86_64",
        modules={},
    )
    assert KernelOpenQA._result_matrix([test]) == []
