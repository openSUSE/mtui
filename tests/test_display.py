from io import StringIO

from mtui.cli import display
from mtui.types import ExecutionMode, System


class MockSystem(System):
    def __init__(self, name):
        self.name = name

    def __str__(self):
        return self.name


def test_println():
    """Test println."""
    output = StringIO()
    d = display.CommandPromptDisplay(output)
    d.println("test message")
    assert output.getvalue() == "test message\n"


def test_list_bugs():
    """Test list_bugs."""
    output = StringIO()
    d = display.CommandPromptDisplay(output)
    bugs = {"123": "Test bug"}
    jira = {"ABC-123": "Test Jira issue"}
    url = "https://bugzilla.suse.com"
    d.list_bugs(bugs, jira, url)
    output_str = output.getvalue()
    assert "Bug #123" in output_str
    assert "Jira #ABC-123" in output_str


def test_list_history():
    """Test list_history."""
    output = StringIO()
    d = display.CommandPromptDisplay(output)
    system = MockSystem("test_system")
    lines = ["1678886400:user:test command"]
    d.list_history("test_host", system, lines)
    output_str = output.getvalue()
    assert "history from test_host" in output_str
    assert "test command" in output_str


def test_list_history_skips_malformed_lines():
    """A torn/hand-edited log line is skipped, not fatal.

    /var/log/mtui.log is appended to over sftp by independent mtui
    sessions; interleaved writes can tear a line so its first field is
    not a parseable timestamp. One bad line used to raise ValueError out
    of list_history, aborting the whole command and dropping every
    remaining host's history.
    """
    output = StringIO()
    d = display.CommandPromptDisplay(output)
    system = MockSystem("test_system")
    lines = [
        "1678886400:user:good before",
        "update:foo: 1720166400:jdoe:install nginx",  # torn: non-numeric field
        "no-colons-at-all",  # too few fields
        "99999999999999999999:user:absurd timestamp",  # fromtimestamp overflow
        "1678886401:user:good after",
    ]
    d.list_history("test_host", system, lines)
    output_str = output.getvalue()
    assert "good before" in output_str
    assert "good after" in output_str
    assert "install nginx" not in output_str


def test_list_host():
    """Test list_host."""
    output = StringIO()
    d = display.CommandPromptDisplay(output)
    system = MockSystem("test_system")
    d.list_host("test_host", system, False, "enabled", ExecutionMode.PARALLEL)
    output_str = output.getvalue()
    assert "test_host" in output_str
    assert "Enabled" in output_str


class MockLock:
    def __init__(self, locked, mine, by, time, comment):
        self._locked = locked
        self._mine = mine
        self._by = by
        self._time = time
        self._comment = comment

    def is_locked(self):
        return self._locked

    def is_mine(self):
        return self._mine

    def locked_by(self):
        return self._by

    def time(self):
        return self._time

    def comment(self):
        return self._comment


def test_list_locks():
    """Test list_locks."""
    output = StringIO()
    d = display.CommandPromptDisplay(output)
    system = MockSystem("test_system")
    lock = MockLock(True, True, "me", "now", "test comment")
    d.list_locks("test_host", system, lock)
    output_str = output.getvalue()
    assert "since now by me" in output_str
    assert "test comment" in output_str


def test_list_sessions():
    """Test list_sessions."""
    output = StringIO()
    d = display.CommandPromptDisplay(output)
    system = MockSystem("test_system")
    d.list_sessions("test_host", system, "test session")
    output_str = output.getvalue()
    assert "sessions on test_host" in output_str
    assert "test session" in output_str


def test_list_timeout():
    """Test list_timeout."""
    output = StringIO()
    d = display.CommandPromptDisplay(output)
    system = MockSystem("test_system")
    d.list_timeout("test_host", system, 600)
    output_str = output.getvalue()
    assert "600s" in output_str


def test_show_log():
    """Test show_log."""
    output = StringIO()

    def sink(msg):
        output.write(msg + "\n")

    display.CommandPromptDisplay.show_log(
        "test_host", [("cmd", "stdout", "stderr", 0, None)], sink
    )
    output_str = output.getvalue()
    assert "log from test_host" in output_str
    assert "stdout" in output_str
    assert "stderr" in output_str
