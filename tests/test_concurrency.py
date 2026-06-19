"""Tests for :class:`mtui.support.concurrency.ContextExecutor`.

The contract these tests pin:

* a :class:`~contextvars.ContextVar` set on the submitting thread is
  visible inside the worker task (a plain ``ThreadPoolExecutor`` would
  read the default);
* each ``submit`` snapshots the context at call time, not at construction;
* worker exceptions still propagate via ``Future.result`` (inherited
  ``ThreadPoolExecutor`` behaviour is preserved through the override).
"""

from __future__ import annotations

import contextvars
import threading

import pytest

from mtui.support.concurrency import ContextExecutor

_var: contextvars.ContextVar[str] = contextvars.ContextVar(
    "test_var", default="DEFAULT"
)


def test_context_var_propagates_into_worker() -> None:
    """A ContextVar set before submit is readable inside the worker."""
    _var.set("SET-BY-CALLER")

    def read_var() -> str:
        return _var.get()

    with ContextExecutor(max_workers=2) as ex:
        result = ex.submit(read_var).result()

    assert result == "SET-BY-CALLER"


def test_worker_runs_on_a_different_thread() -> None:
    """Sanity: the task really runs on a pool thread, not inline."""
    caller = threading.get_ident()

    def worker() -> int:
        return threading.get_ident()

    with ContextExecutor(max_workers=1) as ex:
        worker_tid = ex.submit(worker).result()

    assert worker_tid != caller


def test_each_submit_snapshots_context_at_call_time() -> None:
    """Two submits see the value current at their own submit call."""

    def read_var() -> str:
        return _var.get()

    with ContextExecutor(max_workers=1) as ex:
        _var.set("first")
        f1 = ex.submit(read_var)
        _var.set("second")
        f2 = ex.submit(read_var)
        r1, r2 = f1.result(), f2.result()

    assert r1 == "first"
    assert r2 == "second"


def test_default_when_no_context_var_set() -> None:
    """With nothing set in the context, the worker reads the default.

    Runs inside a brand-new :class:`contextvars.Context` (which starts
    empty) so the assertion is independent of any value other tests left
    on the main thread's context.
    """

    def run_in_fresh_context() -> str:
        def read_var() -> str:
            return _var.get()

        with ContextExecutor(max_workers=1) as ex:
            return ex.submit(read_var).result()

    fresh = contextvars.Context()
    assert fresh.run(run_in_fresh_context) == "DEFAULT"


def test_worker_exception_propagates_via_result() -> None:
    """A worker exception is re-raised by ``Future.result`` (inherited)."""

    def boom() -> None:
        raise ValueError("worker failed")

    with ContextExecutor(max_workers=1) as ex:
        future = ex.submit(boom)
        with pytest.raises(ValueError, match="worker failed"):
            future.result()
