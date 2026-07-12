from unittest.mock import mock_open

from mtui.support import systemcheck


def test_detect_system(monkeypatch):
    """Test detect_system."""
    mock_os_release = 'NAME="test_distro"\nVERSION_ID="test_verid"'
    mock_version = "Linux version test_kernel"

    def mock_open_side_effect(path, mode="r", encoding="utf-8"):
        if path == "/etc/os-release":
            return mock_open(read_data=mock_os_release).return_value
        if path == "/proc/version":
            return mock_open(read_data=mock_version).return_value
        raise FileNotFoundError

    monkeypatch.setattr("builtins.open", mock_open_side_effect)

    distro, verid, kernel = systemcheck.detect_system()

    assert distro == "test_distro"
    assert verid == "test_verid"
    assert kernel == "test_kernel"


def test_detect_system_single_quoted_os_release(monkeypatch):
    """Single-quoted os-release values (spec-legal) must parse too.

    The regexes used ["|] -- inside a character class | is a literal
    pipe, not alternation -- so NAME='openSUSE' never matched and the
    export footer rendered without distro/version.
    """
    mock_os_release = "NAME='openSUSE Tumbleweed'\nVERSION_ID='20260710'"
    mock_version = "Linux version 6.12.0"

    def mock_open_side_effect(path, mode="r", encoding="utf-8"):
        if path == "/etc/os-release":
            return mock_open(read_data=mock_os_release).return_value
        if path == "/proc/version":
            return mock_open(read_data=mock_version).return_value
        raise FileNotFoundError

    monkeypatch.setattr("builtins.open", mock_open_side_effect)

    distro, verid, kernel = systemcheck.detect_system()

    assert distro == "openSUSE Tumbleweed"
    assert verid == "20260710"
    assert kernel == "6.12.0"


def test_detect_system_unquoted_os_release(monkeypatch):
    """Bare (unquoted) os-release values must parse too.

    The os-release spec allows values without spaces to be written
    unquoted (Fedora ships NAME=Fedora); the old regexes required
    quotes, so such files silently yielded an empty distro/version in
    the export footer and commit line.
    """
    mock_os_release = "NAME=Fedora\nVERSION_ID=42"
    mock_version = "Linux version 6.12.0"

    def mock_open_side_effect(path, mode="r", encoding="utf-8"):
        if path == "/etc/os-release":
            return mock_open(read_data=mock_os_release).return_value
        if path == "/proc/version":
            return mock_open(read_data=mock_version).return_value
        raise FileNotFoundError

    monkeypatch.setattr("builtins.open", mock_open_side_effect)

    distro, verid, kernel = systemcheck.detect_system()

    assert distro == "Fedora"
    assert verid == "42"
    assert kernel == "6.12.0"


def test_detect_system_mixed_quoting_os_release(monkeypatch):
    """Quoted and bare values may be mixed in one os-release file."""
    mock_os_release = 'NAME="SLES"\nVERSION_ID=15.6'
    mock_version = "Linux version 6.4.0"

    def mock_open_side_effect(path, mode="r", encoding="utf-8"):
        if path == "/etc/os-release":
            return mock_open(read_data=mock_os_release).return_value
        if path == "/proc/version":
            return mock_open(read_data=mock_version).return_value
        raise FileNotFoundError

    monkeypatch.setattr("builtins.open", mock_open_side_effect)

    distro, verid, kernel = systemcheck.detect_system()

    assert distro == "SLES"
    assert verid == "15.6"
    assert kernel == "6.4.0"


def test_detect_system_escaped_quotes_in_quoted_value(monkeypatch):
    """Quoted values with spec-legal backslash-escaped inner quotes must
    still parse (byte-identical to the pre-unquoted-value-support regex),
    not silently fail to match.

    A stricter quoted alternative (e.g. ``[^"]*`` instead of a greedy
    ``.*``) stops at the first embedded quote and then fails to match at
    all, leaving distro/verid empty -- the same silent-empty-footer bug
    this branch otherwise fixes.
    """
    mock_os_release = 'NAME="SUSE \\"Leap\\""\nVERSION_ID="15.6"'
    mock_version = "Linux version 6.12.0"

    def mock_open_side_effect(path, mode="r", encoding="utf-8"):
        if path == "/etc/os-release":
            return mock_open(read_data=mock_os_release).return_value
        if path == "/proc/version":
            return mock_open(read_data=mock_version).return_value
        raise FileNotFoundError

    monkeypatch.setattr("builtins.open", mock_open_side_effect)

    distro, verid, kernel = systemcheck.detect_system()

    assert distro == 'SUSE \\"Leap\\"'
    assert verid == "15.6"
    assert kernel == "6.12.0"


def test_system_info_default_prefix():
    """The export footer keeps the ``## export`` prefix."""
    s = systemcheck.system_info("openSUSE Leap", "16.0", "6.12.0", "mpluskal")
    assert s.startswith("## export MTUI:")
    assert "on openSUSE Leap-16.0 (kernel: 6.12.0) by mpluskal" in s
    assert s.endswith("\n")


def test_system_info_custom_prefix():
    """A custom prefix (e.g. for commit messages) replaces ``## export``."""
    s = systemcheck.system_info(
        "openSUSE Leap", "16.0", "6.12.0", "mpluskal", prefix="committed from"
    )
    assert s.startswith("committed from MTUI:")
    assert "on openSUSE Leap-16.0 (kernel: 6.12.0) by mpluskal" in s
