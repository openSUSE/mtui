"""Focused tests for the ``Reporter`` collaborator.

These tests cover the seven sink-dispatch methods extracted from
:class:`mtui.hosts.target.Target` (Phase 5b / C2 / Cluster B). They mirror
the contracts the deleted ``Target.report_*`` methods used to advertise
and are reached through the ``Target.reporter`` property so the
property's own behaviour (fresh-per-access binding) is covered
implicitly by every test that asserts against ``target.reporter``.
"""

from unittest.mock import MagicMock

from mtui.hosts.target.reporter import Reporter
from mtui.types import HostLog


def test_reporter_property_returns_reporter_bound_to_target(mock_target):
    """``Target.reporter`` returns a ``Reporter`` keeping a live ref to the target."""
    r = mock_target.reporter
    assert isinstance(r, Reporter)
    assert r.target is mock_target


def test_reporter_property_returns_fresh_instance_each_access(mock_target):
    """Per design the property allocates per access; pin that explicitly."""
    assert mock_target.reporter is not mock_target.reporter


def test_self_calls_sink_with_full_status_tuple(mock_target):
    """``self_`` forwards (hostname, system, transactional, state, mode)."""
    sink = MagicMock()
    mock_target.reporter.self_(sink)
    sink.assert_called_once_with(
        mock_target.hostname,
        mock_target.system,
        mock_target.transactional,
        mock_target.state,
        mock_target.mode,
    )


def test_history_splits_lastout_on_newline(mock_target):
    """``history`` forwards the last stdout as a ``\\n``-split list."""
    sink = MagicMock()
    mock_target.out = HostLog()
    mock_target.out.append(["cmd", "line1\nline2", "", 0, 0])
    mock_target.reporter.history(sink)
    sink.assert_called_once()
    args = sink.call_args[0]
    assert args[0] == mock_target.hostname
    assert args[1] is mock_target.system
    assert args[2] == ["line1", "line2"]


def test_locks_calls_sink_with_underlying_lock_object(mock_target):
    """``locks`` exposes the private ``_lock`` to the sink."""
    sink = MagicMock()
    mock_target.reporter.locks(sink)
    sink.assert_called_once_with(
        mock_target.hostname, mock_target.system, mock_target._lock
    )


def test_timeout_reads_connection_timeout_at_call_time(mock_target):
    """``timeout`` reflects the live ``connection.timeout`` value."""
    mock_target.connection.timeout = 42
    sink = MagicMock()
    mock_target.reporter.timeout(sink)
    sink.assert_called_once_with(mock_target.hostname, mock_target.system, 42)


def test_sessions_forwards_full_lastout_string(mock_target):
    """``sessions`` forwards the last stdout verbatim (used by ``who``)."""
    sink = MagicMock()
    mock_target.out = HostLog()
    mock_target.out.append(["who", "alice tty1\n", "", 0, 0])
    mock_target.reporter.sessions(sink)
    sink.assert_called_once_with(
        mock_target.hostname, mock_target.system, "alice tty1\n"
    )


def test_log_passes_full_outlog_and_extra_arg(mock_target):
    """``log`` forwards the full ``out`` log plus a caller-provided extra."""
    sink = MagicMock()
    mock_target.out = HostLog()
    mock_target.reporter.log(sink, "some-arg")
    sink.assert_called_once_with(mock_target.hostname, mock_target.out, "some-arg")


def test_products_calls_sink_with_hostname_and_system(mock_target):
    """``products`` is a minimal ``(hostname, system)`` sink."""
    sink = MagicMock()
    mock_target.reporter.products(sink)
    sink.assert_called_once_with(mock_target.hostname, mock_target.system)
