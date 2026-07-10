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
