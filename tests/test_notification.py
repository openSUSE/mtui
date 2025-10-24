import pytest
from mtui import notification
from unittest.mock import patch, MagicMock

@patch('mtui.notification.__impl', new_callable=MagicMock)
def test_display(mock_pynotify):
    """
    Test display
    """
    # Test that the notification is displayed
    notification.display("test summary", "test text")
    mock_pynotify.Notification.assert_called_with("test summary", "test text", "stock_dialog-info")
    mock_pynotify.Notification.return_value.show.assert_called_once()

@patch('mtui.notification.__impl', None)
def test_display_no_pynotify():
    """
    Test display when pynotify is not installed
    """
    # Test that no error is raised
    notification.display("test summary", "test text")

@patch('mtui.notification.__impl', new_callable=MagicMock)
def test_display_init_fails(mock_pynotify):
    """
    Test display when pynotify fails to initialize
    """
    mock_pynotify.init.return_value = False
    # Test that no error is raised
    notification.display("test summary", "test text")
