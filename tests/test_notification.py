import pytest
from mtui import notification
from unittest.mock import MagicMock
import sys

def test_display_success(monkeypatch):
    """
    Test display when pynotify is available and works.
    """
    mock_pynotify = MagicMock()
    # Bypass the import/init logic by setting __impl directly
    monkeypatch.setattr(notification, '__impl', mock_pynotify)

    notification.display("test summary", "test text")

    mock_pynotify.Notification.assert_called_with("test summary", "test text", "stock_dialog-info")
    mock_pynotify.Notification.return_value.show.assert_called_once()

def test_display_import_error(monkeypatch):
    """
    Test display when pynotify is not installed.
    """
    # Reset state to ensure the import logic is triggered
    monkeypatch.setattr(notification, '__impl', None)
    # Make the import fail
    monkeypatch.setitem(sys.modules, 'pynotify', None)

    # Should execute quietly without raising an exception
    notification.display("test summary", "test text")

def test_display_init_fails(monkeypatch):
    """
    Test display when pynotify is installed but fails to initialize.
    """
    # Reset state to ensure the import logic is triggered
    monkeypatch.setattr(notification, '__impl', None)

    mock_pynotify = MagicMock()
    mock_pynotify.init.return_value = False

    # Make the import succeed but return our mock
    monkeypatch.setitem(sys.modules, 'pynotify', mock_pynotify)

    notification.display("test summary", "test text")

    # Assert that initialization was attempted but showing a notification was not
    mock_pynotify.init.assert_called_once_with("mtui")
    mock_pynotify.Notification.assert_not_called()
