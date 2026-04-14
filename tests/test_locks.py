"""Tests for the mtui target locks module."""

import errno
import os
from unittest.mock import MagicMock

import pytest

from mtui.target.locks import LockedTargets, RemoteLock, TargetLock, TargetLockedError


# --- RemoteLock ---


class TestRemoteLock:
    def test_default_init(self):
        """Test RemoteLock default state."""
        rl = RemoteLock()
        assert rl.user == ""
        assert rl.timestamp == ""
        assert rl.pid == 0
        assert rl.comment == ""

    def test_to_lockfile_without_comment(self):
        """Test lockfile serialization without comment."""
        rl = RemoteLock()
        rl.timestamp = "1700000000"
        rl.user = "testuser"
        rl.pid = 12345

        result = rl.to_lockfile()
        assert result == "1700000000:testuser:12345"

    def test_to_lockfile_with_comment(self):
        """Test lockfile serialization with comment."""
        rl = RemoteLock()
        rl.timestamp = "1700000000"
        rl.user = "testuser"
        rl.pid = 12345
        rl.comment = "update in progress"

        result = rl.to_lockfile()
        assert result == "1700000000:testuser:12345:update in progress"

    def test_from_lockfile_empty(self):
        """Test parsing empty lockfile line."""
        rl = RemoteLock.from_lockfile("")
        assert rl.user == ""
        assert rl.pid == 0

    def test_from_lockfile_basic(self):
        """Test parsing a valid lockfile line."""
        rl = RemoteLock.from_lockfile("1700000000:testuser:12345")
        assert rl.timestamp == "1700000000"
        assert rl.user == "testuser"
        assert rl.pid == 12345
        assert rl.comment == ""

    def test_from_lockfile_with_comment(self):
        """Test parsing lockfile line with comment."""
        rl = RemoteLock.from_lockfile("1700000000:testuser:12345:my comment")
        assert rl.comment == "my comment"

    def test_from_lockfile_comment_with_colons(self):
        """Test parsing lockfile line where comment contains colons."""
        rl = RemoteLock.from_lockfile("1700000000:user:999:comment:with:colons")
        assert rl.user == "user"
        assert rl.pid == 999
        assert rl.comment == "comment:with:colons"

    def test_from_lockfile_too_few_fields(self):
        """Test parsing lockfile line with too few fields raises ValueError."""
        with pytest.raises(ValueError, match="weird format"):
            RemoteLock.from_lockfile("only:one")

    def test_str_with_comment(self):
        """Test string representation with comment."""
        rl = RemoteLock()
        rl.user = "testuser"
        rl.comment = "testing"

        assert "locked by testuser (testing)" in str(rl)

    def test_str_without_comment(self):
        """Test string representation without comment."""
        rl = RemoteLock()
        rl.user = "testuser"

        assert "locked by testuser." in str(rl)

    def test_roundtrip(self):
        """Test that to_lockfile -> from_lockfile roundtrip preserves data."""
        original = RemoteLock()
        original.timestamp = "1700000000"
        original.user = "admin"
        original.pid = 42
        original.comment = "test comment"

        parsed = RemoteLock.from_lockfile(original.to_lockfile())

        assert parsed.timestamp == original.timestamp
        assert parsed.user == original.user
        assert parsed.pid == original.pid
        assert parsed.comment == original.comment


# --- TargetLock ---


class TestTargetLock:
    @pytest.fixture
    def lock(self, mock_config):
        """Create a TargetLock with mocked connection."""
        mock_config.session_user = "testuser"
        conn = MagicMock()
        conn.hostname = "host1.example.com"
        return TargetLock(conn, mock_config)

    def test_init(self, lock):
        """Test TargetLock initialization."""
        assert lock.i_am_user == "testuser"
        assert lock.i_am_pid == os.getpid()

    def test_is_locked_when_unlocked(self, lock):
        """Test is_locked returns False when no lockfile exists."""
        lock.connection.sftp_open.side_effect = OSError(errno.ENOENT, "not found")
        assert lock.is_locked() is False

    def test_is_locked_when_locked(self, lock):
        """Test is_locked returns True when lockfile contains data."""
        mock_file = MagicMock()
        mock_file.readline.return_value = "1700000000:otheruser:99999"
        lock.connection.sftp_open.return_value = mock_file

        assert lock.is_locked() is True

    def test_lock_creates_lockfile(self, lock):
        """Test lock() creates a lockfile on the remote host."""
        # First call to is_locked (in lock()) should return False
        lock.connection.sftp_open.side_effect = [
            OSError(errno.ENOENT, "not found"),  # is_locked -> load
            MagicMock(),  # the actual lockfile write
        ]

        lock.lock("test comment")

        # Verify sftp_open was called with write mode
        calls = lock.connection.sftp_open.call_args_list
        assert len(calls) == 2
        assert calls[1][0][1] == "w+"

    def test_lock_raises_when_locked_by_other(self, lock):
        """Test lock() raises TargetLockedError when locked by another user."""
        mock_file = MagicMock()
        mock_file.readline.return_value = "1700000000:otheruser:99999"
        lock.connection.sftp_open.return_value = mock_file

        with pytest.raises(TargetLockedError):
            lock.lock()

    def test_lock_allows_relock_by_same_user(self, lock):
        """Test lock() allows re-locking when lock is owned by current user."""
        mock_file = MagicMock()
        mock_file.readline.return_value = f"1700000000:testuser:{os.getpid()}"
        lock.connection.sftp_open.return_value = mock_file

        lock.lock("new comment")  # should not raise

    def test_unlock_when_not_locked(self, lock):
        """Test unlock() does nothing when not locked."""
        lock.connection.sftp_open.side_effect = OSError(errno.ENOENT, "not found")
        lock.unlock()  # should not raise or call sftp_remove

    def test_unlock_removes_lockfile(self, lock):
        """Test unlock() removes the lockfile when locked by current user."""
        mock_file = MagicMock()
        mock_file.readline.return_value = f"1700000000:testuser:{os.getpid()}"
        lock.connection.sftp_open.return_value = mock_file

        lock.unlock()

        lock.connection.sftp_remove.assert_called_once()

    def test_unlock_raises_when_locked_by_other(self, lock):
        """Test unlock() raises TargetLockedError when locked by another user."""
        mock_file = MagicMock()
        mock_file.readline.return_value = "1700000000:otheruser:99999"
        lock.connection.sftp_open.return_value = mock_file

        with pytest.raises(TargetLockedError):
            lock.unlock()

    def test_unlock_force_removes_others_lock(self, lock):
        """Test unlock(force=True) removes lock even when owned by another user."""
        mock_file = MagicMock()
        mock_file.readline.return_value = "1700000000:otheruser:99999"
        lock.connection.sftp_open.return_value = mock_file

        lock.unlock(force=True)

        lock.connection.sftp_remove.assert_called_once()

    def test_is_mine_true(self, lock):
        """Test is_mine() returns True when lock is owned by current user."""
        lock._lock = RemoteLock()
        lock._lock.user = "testuser"
        lock._lock.pid = os.getpid()

        assert lock.is_mine() is True

    def test_is_mine_false_different_user(self, lock):
        """Test is_mine() returns False for different user."""
        lock._lock = RemoteLock()
        lock._lock.user = "otheruser"
        lock._lock.pid = os.getpid()

        assert lock.is_mine() is False

    def test_is_mine_false_different_pid(self, lock):
        """Test is_mine() returns False for same user but different pid."""
        lock._lock = RemoteLock()
        lock._lock.user = "testuser"
        lock._lock.pid = os.getpid() + 1

        assert lock.is_mine() is False

    def test_is_mine_raises_when_not_locked(self, lock):
        """Test is_mine() raises RuntimeError when not locked."""
        lock._lock = RemoteLock()  # user is empty

        with pytest.raises(RuntimeError, match="not locked"):
            lock.is_mine()

    def test_locked_by_msg(self, lock):
        """Test locked_by_msg() returns formatted message."""
        mock_file = MagicMock()
        mock_file.readline.return_value = "1700000000:testuser:12345:testing"
        lock.connection.sftp_open.return_value = mock_file

        msg = lock.locked_by_msg()
        assert "host1.example.com" in msg
        assert "testuser" in msg

    def test_time_returns_formatted_time(self, lock):
        """Test time() returns a formatted timestamp string."""
        mock_file = MagicMock()
        mock_file.readline.return_value = "1700000000:testuser:12345"
        lock.connection.sftp_open.return_value = mock_file

        result = lock.time()
        assert "UTC" in result


# --- LockedTargets context manager ---


class TestLockedTargets:
    def test_enter_locks_all(self):
        """Test __enter__ locks all targets."""
        t1 = MagicMock()
        t2 = MagicMock()

        with LockedTargets([t1, t2]):
            t1.lock.assert_called_once()
            t2.lock.assert_called_once()

    def test_exit_unlocks_all(self):
        """Test __exit__ unlocks all targets."""
        t1 = MagicMock()
        t2 = MagicMock()

        with LockedTargets([t1, t2]):
            pass

        t1.unlock.assert_called_once()
        t2.unlock.assert_called_once()

    def test_exit_unlocks_on_exception(self):
        """Test __exit__ unlocks targets even when exception occurs."""
        t1 = MagicMock()

        with pytest.raises(ValueError), LockedTargets([t1]):
            raise ValueError("test error")

        t1.unlock.assert_called_once()
