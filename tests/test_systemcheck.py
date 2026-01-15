from unittest.mock import mock_open

from mtui import systemcheck


def test_detect_system(monkeypatch):
    """
    Test detect_system
    """
    mock_os_release = 'NAME="test_distro"\nVERSION_ID="test_verid"'
    mock_version = "Linux version test_kernel"

    def mock_open_side_effect(path, mode="r", encoding="utf-8"):
        if path == "/etc/os-release":
            return mock_open(read_data=mock_os_release).return_value
        elif path == "/proc/version":
            return mock_open(read_data=mock_version).return_value
        else:
            raise FileNotFoundError

    monkeypatch.setattr("builtins.open", mock_open_side_effect)

    distro, verid, kernel = systemcheck.detect_system()

    assert distro == "test_distro"
    assert verid == "test_verid"
    assert kernel == "test_kernel"
