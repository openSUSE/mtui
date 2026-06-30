"""Tests for the non-raising pool claim path: ``TargetLock.try_claim``."""

import errno
import os
import time
from unittest.mock import MagicMock

import pytest

from mtui.hosts.target.locks import TargetLock


@pytest.fixture
def lock(mock_config):
    """A TargetLock with mocked connection and stale-reaping enabled."""
    mock_config.session_user = "testuser"
    mock_config.lock_reap_stale = True
    mock_config.lock_stale_age = 86400
    conn = MagicMock()
    conn.hostname = "host1.example.com"
    return TargetLock(conn, mock_config)


def _locked_by(contents: str) -> MagicMock:
    """Return a mock lockfile whose readline yields ``contents``."""
    f = MagicMock()
    f.readline.return_value = contents
    return f


def test_try_claim_free_host(lock):
    """A free host is claimed (True) via the atomic exclusive create."""
    # try_claim's is_locked()->load (ENOENT), then lock()'s exclusive ``"x"``
    # create wins outright (no stat-before-write).
    lock.connection.sftp_open.side_effect = [
        OSError(errno.ENOENT, "not found"),  # try_claim is_locked
        MagicMock(),  # lock() exclusive "x" create succeeds
    ]
    assert lock.try_claim("mtui pool RRID [owner]") is True
    # The write that won was the exclusive create.
    assert lock.connection.sftp_open.call_args_list[-1][0][1] == "x"


def test_try_claim_foreign_host_returns_false(lock):
    """A host locked by someone else returns False, never raises."""
    fresh = int(time.time()) - 60  # not stale
    lock.connection.sftp_open.return_value = _locked_by(f"{fresh}:otheruser:99999")
    assert lock.try_claim() is False
    lock.connection.sftp_remove.assert_not_called()


def test_try_claim_mine_relocks(lock):
    """A host already locked by us is re-claimed (True)."""
    lock.connection.sftp_open.return_value = _locked_by(
        f"{int(time.time())}:testuser:{os.getpid()}"
    )
    assert lock.try_claim("new comment") is True


def test_try_claim_stale_reaps_then_claims(lock):
    """A stale foreign lock is reaped and then claimed."""
    stale = int(time.time()) - 200000  # > 1 day
    # try_claim: is_locked->load(stale); is_mine->False (cached);
    # reap_if_stale: age_seconds->load(stale), unlock(force): is_locked->load,
    # sftp_remove; then lock()'s exclusive "x" create wins on the now-free host.
    lock.connection.sftp_open.side_effect = [
        _locked_by(f"{stale}:otheruser:99999"),  # try_claim is_locked
        _locked_by(f"{stale}:otheruser:99999"),  # reap age_seconds load
        _locked_by(f"{stale}:otheruser:99999"),  # unlock(force) is_locked load
        MagicMock(),  # lock() exclusive "x" create succeeds
    ]
    assert lock.try_claim() is True
    lock.connection.sftp_remove.assert_called_once()
