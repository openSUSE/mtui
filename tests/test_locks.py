"""Tests for the mtui target locks module."""

import errno
import os
import time
from unittest.mock import MagicMock

import pytest

from mtui.hosts.target.locks import (
    LockedTargets,
    PoolLock,
    RemoteLock,
    TargetLock,
    TargetLockedError,
)

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
        """Test lock() creates the lockfile via an atomic exclusive create."""
        # On a free host the exclusive ``"x"`` create succeeds outright; there
        # is no preceding stat, so a single sftp_open call (mode "x") is made.
        lock.connection.sftp_open.return_value = MagicMock()

        lock.lock("test comment")

        calls = lock.connection.sftp_open.call_args_list
        assert len(calls) == 1
        assert calls[0][0][1] == "x"

    def test_lock_raises_when_locked_by_other(self, lock):
        """Test lock() raises TargetLockedError when locked by another user."""
        mock_file = MagicMock()
        mock_file.readline.return_value = "1700000000:otheruser:99999"
        # Exclusive create loses the race (file exists); subsequent reads see
        # the foreign lock and lock() must refuse.
        lock.connection.sftp_open.side_effect = _busy_sftp_open(mock_file)

        with pytest.raises(TargetLockedError):
            lock.lock()

    def test_lock_allows_relock_by_same_user(self, lock):
        """Test lock() allows re-locking when lock is owned by current user."""
        mock_file = MagicMock()
        mock_file.readline.return_value = f"1700000000:testuser:{os.getpid()}"
        # Exclusive create loses (file exists) but it is our own lock, so the
        # reconciliation overwrites it with the new comment instead of raising.
        lock.connection.sftp_open.side_effect = _busy_sftp_open(mock_file)

        lock.lock("new comment")  # should not raise

    def test_lock_uses_atomic_exclusive_create_on_free_host(self, lock):
        """On a free host the very first open is the atomic exclusive create.

        No preceding stat/read happens, so the read-then-write TOCTOU window is
        gone: a single ``sftp_open(.., "x")`` either wins the create or the
        process loses the race and reconciles.
        """
        lock.connection.sftp_open.return_value = MagicMock()

        lock.lock("mtui pool RRID [owner]")

        first_call = lock.connection.sftp_open.call_args_list[0]
        assert first_call[0][1] == "x"

    def test_try_claim_loses_atomic_race_to_concurrent_winner(self, lock):
        """A second concurrent exclusive claim sees the file exist and backs off.

        Models two processes racing: the winner created the lockfile, so this
        caller's ``"x"`` create raises ``FileExistsError`` and the foreign lock
        is fresh (not reapable), so ``try_claim`` returns ``False`` instead of
        clobbering the winner.
        """
        lock.config.lock_wait = 0
        lock.config.lock_reap_stale = False
        fresh = int(time.time()) - 60
        lock.connection.sftp_open.side_effect = _busy_sftp_open(
            _file(f"{fresh}:otheruser:99999")
        )

        assert lock.try_claim("mtui pool RRID [me]") is False

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

    def test_time_converts_epoch_to_utc(self, lock):
        """time() renders the epoch timestamp in UTC, as its label claims.

        The exact string is asserted: a timezone-aware UTC conversion yields
        the same result on any host, whereas a naive (local-time) conversion
        would only match on hosts whose local timezone happens to be UTC.
        """
        mock_file = MagicMock()
        mock_file.readline.return_value = "0:testuser:12345"
        lock.connection.sftp_open.return_value = mock_file

        assert lock.time() == "Thursday, 01.01.1970 00:00 UTC"

    def test_time_is_utc_regardless_of_local_timezone(self, lock, monkeypatch):
        """time() must not shift with the tester's local timezone.

        Locks are shared between testers in different timezones, so the
        displayed time must be canonical UTC. With TZ=America/New_York a
        naive local-time conversion of 1720000000 would render 05:46 (EDT,
        UTC-4) under the hardcoded "UTC" label instead of the true 09:46.
        """
        mock_file = MagicMock()
        mock_file.readline.return_value = "1720000000:testuser:12345"
        lock.connection.sftp_open.return_value = mock_file

        monkeypatch.setenv("TZ", "America/New_York")
        time.tzset()
        try:
            assert lock.time() == "Wednesday, 03.07.2024 09:46 UTC"
        finally:
            # monkeypatch restores TZ only at teardown; re-apply the process
            # timezone here so no stale zone leaks into later tests.
            monkeypatch.undo()
            time.tzset()

    def test_time_malformed_timestamp_returns_unknown(self, lock):
        """A non-numeric timestamp must not raise (mirrors age_seconds).

        time() is called from update_lock while reporting a foreign lock; a
        bad timestamp there must not abort the whole lock-acquisition walk.
        """
        mock_file = MagicMock()
        mock_file.readline.return_value = "notanumber:otheruser:99999"
        lock.connection.sftp_open.return_value = mock_file

        assert lock.time() == "unknown"

    def test_time_empty_timestamp_returns_unknown(self, lock):
        """An empty timestamp field is handled gracefully too."""
        mock_file = MagicMock()
        mock_file.readline.return_value = ":otheruser:99999"
        lock.connection.sftp_open.return_value = mock_file

        assert lock.time() == "unknown"


# --- Stale lock reaping ---


class TestTargetLockReaping:
    @pytest.fixture
    def lock(self, mock_config):
        """A TargetLock with stale-reaping enabled (1 day threshold)."""
        mock_config.session_user = "testuser"
        mock_config.lock_reap_stale = True
        mock_config.lock_stale_age = 86400
        conn = MagicMock()
        conn.hostname = "host1.example.com"
        return TargetLock(conn, mock_config)

    @staticmethod
    def _set_lockfile(lock, contents):
        """Make the mocked remote return a lockfile with the given line."""
        mock_file = MagicMock()
        mock_file.readline.return_value = contents
        lock.connection.sftp_open.return_value = mock_file

    def test_age_seconds_none_when_unlocked(self, lock):
        """age_seconds() returns None when no lockfile exists."""
        lock.connection.sftp_open.side_effect = OSError(errno.ENOENT, "not found")
        assert lock.age_seconds() is None

    def test_age_seconds_none_on_malformed_timestamp(self, lock):
        """age_seconds() returns None for a non-numeric timestamp."""
        self._set_lockfile(lock, "not-a-number:otheruser:99999")
        assert lock.age_seconds() is None

    def test_age_seconds_computes_age(self, lock):
        """age_seconds() returns roughly the elapsed time since the lock."""
        old = int(time.time()) - 3600
        self._set_lockfile(lock, f"{old}:otheruser:99999")
        age = lock.age_seconds()
        assert age is not None
        assert 3590 <= age <= 3700

    def test_reap_removes_stale_foreign_lock(self, lock):
        """A lock older than the threshold is force-removed regardless of owner."""
        stale = int(time.time()) - 200000  # > 1 day
        self._set_lockfile(lock, f"{stale}:otheruser:99999")

        assert lock.reap_if_stale() is True
        lock.connection.sftp_remove.assert_called_once()

    def test_reap_removes_stale_exclusive_lock(self, lock):
        """Exclusive (commented) locks are reaped too once stale."""
        stale = int(time.time()) - 200000
        self._set_lockfile(lock, f"{stale}:otheruser:99999:do not touch")

        assert lock.reap_if_stale() is True
        lock.connection.sftp_remove.assert_called_once()

    def test_reap_keeps_fresh_lock(self, lock):
        """A lock younger than the threshold is left untouched."""
        fresh = int(time.time()) - 60
        self._set_lockfile(lock, f"{fresh}:otheruser:99999")

        assert lock.reap_if_stale() is False
        lock.connection.sftp_remove.assert_not_called()

    def test_reap_disabled_by_flag(self, lock):
        """reap_if_stale() does nothing when lock_reap_stale is False."""
        lock.config.lock_reap_stale = False
        stale = int(time.time()) - 200000
        self._set_lockfile(lock, f"{stale}:otheruser:99999")

        assert lock.reap_if_stale() is False
        lock.connection.sftp_remove.assert_not_called()

    def test_reap_disabled_by_zero_age(self, lock):
        """A non-positive lock_stale_age disables reaping."""
        lock.config.lock_stale_age = 0
        stale = int(time.time()) - 200000
        self._set_lockfile(lock, f"{stale}:otheruser:99999")

        assert lock.reap_if_stale() is False
        lock.connection.sftp_remove.assert_not_called()

    def test_reap_keeps_malformed_lock(self, lock):
        """A lock with an unparseable timestamp is left untouched."""
        self._set_lockfile(lock, "garbage:otheruser:99999")

        assert lock.reap_if_stale() is False
        lock.connection.sftp_remove.assert_not_called()


# --- [lock] wait queueing ---


class TestTargetLockWait:
    @pytest.fixture
    def lock(self, mock_config):
        mock_config.session_user = "testuser"
        mock_config.lock_reap_stale = True
        mock_config.lock_stale_age = 86400
        mock_config.lock_wait = 0
        mock_config.lock_wait_poll = 15
        conn = MagicMock()
        conn.hostname = "host1.example.com"
        return TargetLock(conn, mock_config)

    def test_wait_disabled_fails_fast(self, lock):
        """wait <= 0 raises immediately on a foreign lock (legacy behaviour)."""
        fresh = int(time.time()) - 60
        lock.connection.sftp_open.side_effect = _busy_sftp_open(
            _locked_by_fresh(lock, fresh)
        )
        with pytest.raises(TargetLockedError):
            lock.lock()

    def test_wait_succeeds_when_freed(self, lock, monkeypatch):
        """With wait > 0, a host that frees up mid-poll is then locked."""
        lock.config.lock_wait = 5
        lock.config.lock_wait_poll = 1
        monkeypatch.setattr(time, "sleep", lambda _s: None)

        fresh = int(time.time()) - 60
        # 1) exclusive "x" create loses the race (file exists); 2) lock()
        # is_locked -> foreign; 3) _wait reap age -> foreign(fresh, not stale);
        # 4) after sleep, is_locked -> ENOENT (freed); 5) lock() "w+" write.
        lock.connection.sftp_open.side_effect = [
            FileExistsError("lockfile exists"),
            _file(f"{fresh}:otheruser:99999"),
            _file(f"{fresh}:otheruser:99999"),
            OSError(errno.ENOENT, "gone"),
            MagicMock(),
        ]
        lock.lock("mtui pool RRID [owner]")  # should not raise

    def test_wait_times_out_then_raises(self, lock):
        """A host that stays busy past the budget still raises.

        Uses a real (sub-second) budget so the monotonic deadline actually
        advances; ``sleep`` is left real but tiny.
        """
        lock.config.lock_wait = 1
        lock.config.lock_wait_poll = 1
        fresh = int(time.time()) - 60
        lock.connection.sftp_open.side_effect = _busy_sftp_open(
            _file(f"{fresh}:otheruser:99999")
        )
        with pytest.raises(TargetLockedError):
            lock.lock()


def _file(contents: str) -> MagicMock:
    f = MagicMock()
    f.readline.return_value = contents
    return f


def _busy_sftp_open(read_file: MagicMock):
    """side_effect making the exclusive ``"x"`` create lose the race.

    Models a host that is already locked: the atomic ``sftp_open(.., "x")`` in
    :meth:`TargetLock.lock` raises ``FileExistsError`` (someone else holds the
    file), and every other open (the reconciliation reads, then the ``"w+"``
    overwrite) returns ``read_file``.
    """

    def _open(_filename, mode="r", *_a, **_k):
        if mode == "x":
            raise FileExistsError("lockfile exists")
        return read_file

    return _open


def _locked_by_fresh(lock, ts: int) -> MagicMock:
    return _file(f"{ts}:otheruser:99999")


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

        with pytest.raises(ValueError, match="test error"), LockedTargets([t1]):
            raise ValueError("test error")

        t1.unlock.assert_called_once()


# --- PoolLock ---


class TestPoolLock:
    @pytest.fixture
    def pool(self, mock_config):
        """A PoolLock owned by template SUSE:Maintenance:1:2, user testuser."""
        mock_config.session_user = "testuser"
        conn = MagicMock()
        conn.hostname = "host1.example.com"
        return PoolLock(conn, mock_config, rrid="SUSE:Maintenance:1:2")

    def test_uses_separate_lockfile(self):
        """The pool lock lives in its own file, distinct from the zypper lock."""
        assert PoolLock.filename != TargetLock.filename
        assert str(PoolLock.filename) == "/var/lock/mtui-pool.lock"

    def test_rrid_of_parses_pool_comment(self):
        """``mtui pool <RRID> [<owner>]`` yields the RRID."""
        assert (
            PoolLock._rrid_of("mtui pool SUSE:Maintenance:1:2 [alice]")
            == "SUSE:Maintenance:1:2"
        )

    def test_rrid_of_non_pool_comment_empty(self):
        """A non-pool comment yields no RRID."""
        assert PoolLock._rrid_of("testing of something") == ""
        assert PoolLock._rrid_of("") == ""

    def test_is_mine_same_rrid_ignores_pid(self, pool):
        """A pool lock by the same template + user is mine regardless of PID."""
        pool._lock = RemoteLock()
        pool._lock.user = "testuser"
        pool._lock.pid = pool.i_am_pid + 1  # different process
        pool._lock.comment = "mtui pool SUSE:Maintenance:1:2 [alice]"
        assert pool.is_mine() is True

    def test_is_mine_different_rrid_not_mine(self, pool):
        """A pool lock by a different template (same user) is not mine."""
        pool._lock = RemoteLock()
        pool._lock.user = "testuser"
        pool._lock.pid = pool.i_am_pid
        pool._lock.comment = "mtui pool SUSE:Maintenance:9:9 [bob]"
        assert pool.is_mine() is False

    def test_is_mine_different_user_not_mine(self, pool):
        """A pool lock by a different user is not mine even with same RRID."""
        pool._lock = RemoteLock()
        pool._lock.user = "otheruser"
        pool._lock.comment = "mtui pool SUSE:Maintenance:1:2 [alice]"
        assert pool.is_mine() is False

    def test_is_mine_no_rrid_falls_back_to_user(self, mock_config):
        """With no session RRID, ownership degrades to user-only."""
        mock_config.session_user = "testuser"
        conn = MagicMock()
        conn.hostname = "host1.example.com"
        pool = PoolLock(conn, mock_config, rrid="")
        pool._lock = RemoteLock()
        pool._lock.user = "testuser"
        pool._lock.comment = "mtui pool SUSE:Maintenance:9:9 [bob]"
        assert pool.is_mine() is True

    def test_is_mine_raises_when_not_locked(self, pool):
        """An unlocked pool lock raises, matching TargetLock's contract."""
        pool._lock = RemoteLock()
        with pytest.raises(RuntimeError, match="not locked"):
            pool.is_mine()
